//! RSA public-key operations for the login encryption exchange.

use rand::rngs::OsRng;
use rsa::pkcs8::DecodePublicKey;
use rsa::{Pkcs1v15Encrypt, RsaPrivateKey, RsaPublicKey};

use crate::core::error::{MineRiderError, Result};

/// Encrypts `data` with an RSA public key given as DER-encoded
/// SubjectPublicKeyInfo, using PKCS#1 v1.5 padding.
///
/// This is what the client sends back in the Encryption Response: the
/// shared secret and the verify token, each encrypted with the server's
/// public key from the Encryption Request.
pub fn encrypt_pkcs1v15(public_key_der: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    let key = RsaPublicKey::from_public_key_der(public_key_der).map_err(|e| {
        MineRiderError::Crypto(format!("invalid server public key (DER): {e}"))
    })?;
    key.encrypt(&mut OsRng, Pkcs1v15Encrypt, data)
        .map_err(|e| MineRiderError::Crypto(format!("RSA encryption failed: {e}")))
}

/// Generates an RSA keypair. Only used by tests and the mock server; real
/// clients never generate their own key.
#[doc(hidden)]
pub fn generate_keypair(bits: usize) -> Result<(RsaPublicKey, RsaPrivateKey)> {
    let private = RsaPrivateKey::new(&mut OsRng, bits)
        .map_err(|e| MineRiderError::Crypto(format!("RSA keygen failed: {e}")))?;
    let public = RsaPublicKey::from(&private);
    Ok((public, private))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsa::pkcs8::EncodePublicKey;

    #[test]
    fn encrypt_with_public_decrypt_with_private() {
        let (public, private) = generate_keypair(1024).unwrap();
        let der = public.to_public_key_der().unwrap();
        let secret = [42u8; 16];
        let ciphertext = encrypt_pkcs1v15(der.as_bytes(), &secret).unwrap();
        assert_ne!(ciphertext, secret);
        let decrypted = private.decrypt(Pkcs1v15Encrypt, &ciphertext).unwrap();
        assert_eq!(decrypted, secret);
    }

    #[test]
    fn invalid_der_errors() {
        assert!(matches!(
            encrypt_pkcs1v15(&[0xDE, 0xAD, 0xBE, 0xEF], &[1, 2, 3]),
            Err(MineRiderError::Crypto(_))
        ));
    }
}
