//! RSA public-key operations for the login encryption exchange.

use rand::rngs::OsRng;
use rsa::pkcs8::DecodePublicKey;
use rsa::traits::PublicKeyParts;
use rsa::{Pkcs1v15Encrypt, RsaPrivateKey, RsaPublicKey};

use crate::error::{ProtocolError, Result};

/// Accepted RSA public-key modulus size range, in bits. Vanilla servers use
/// 1024-bit keys; the range is generous so any real server still works,
/// while a malicious or broken server can no longer force multiple seconds
/// of client CPU per encryption (there are two per login: shared secret and
/// verify token) by advertising an oversized key, nor can a degenerate
/// near-zero-bit key slip through.
const MIN_KEY_BITS: usize = 512;
const MAX_KEY_BITS: usize = 4096;

/// Encrypts `data` with an RSA public key given as DER-encoded
/// SubjectPublicKeyInfo, using PKCS#1 v1.5 padding.
///
/// This is what the client sends back in the Encryption Response: the
/// shared secret and the verify token, each encrypted with the server's
/// public key from the Encryption Request.
pub fn encrypt_pkcs1v15(public_key_der: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    let key = RsaPublicKey::from_public_key_der(public_key_der)
        .map_err(|e| ProtocolError::Crypto(format!("invalid server public key (DER): {e}")))?;
    validate_key_bits(key.size() * 8)?;
    key.encrypt(&mut OsRng, Pkcs1v15Encrypt, data)
        .map_err(|e| ProtocolError::Crypto(format!("RSA encryption failed: {e}")))
}

/// Rejects a server-supplied RSA key size outside [`MIN_KEY_BITS`],
/// [`MAX_KEY_BITS`], before any encryption is attempted against it.
fn validate_key_bits(bits: usize) -> Result<()> {
    if (MIN_KEY_BITS..=MAX_KEY_BITS).contains(&bits) {
        Ok(())
    } else {
        Err(ProtocolError::Crypto(format!(
            "server RSA public key size {bits} bits outside accepted range {MIN_KEY_BITS}-{MAX_KEY_BITS} bits"
        )))
    }
}

/// Generates an RSA keypair. Only used by tests and the mock server; real
/// clients never generate their own key.
#[doc(hidden)]
pub fn generate_keypair(bits: usize) -> Result<(RsaPublicKey, RsaPrivateKey)> {
    let private = RsaPrivateKey::new(&mut OsRng, bits)
        .map_err(|e| ProtocolError::Crypto(format!("RSA keygen failed: {e}")))?;
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
    fn tampered_ciphertext_fails_to_decrypt() {
        let (public, private) = generate_keypair(1024).unwrap();
        let der = public.to_public_key_der().unwrap();
        let secret = [42u8; 16];
        let mut ciphertext = encrypt_pkcs1v15(der.as_bytes(), &secret).unwrap();
        // Flip a bit in the middle of the ciphertext: PKCS#1 v1.5 padding
        // validation must reject it.
        let mid = ciphertext.len() / 2;
        ciphertext[mid] ^= 0x01;
        assert!(
            private.decrypt(Pkcs1v15Encrypt, &ciphertext).is_err(),
            "tampered ciphertext must not decrypt"
        );
        // Truncated ciphertext must fail as well.
        assert!(private
            .decrypt(Pkcs1v15Encrypt, &ciphertext[..mid])
            .is_err());
    }

    #[test]
    fn invalid_der_errors() {
        assert!(matches!(
            encrypt_pkcs1v15(&[0xDE, 0xAD, 0xBE, 0xEF], &[1, 2, 3]),
            Err(ProtocolError::Crypto(_))
        ));
    }

    #[test]
    fn key_bits_boundaries() {
        assert!(validate_key_bits(MIN_KEY_BITS).is_ok());
        assert!(validate_key_bits(MAX_KEY_BITS).is_ok());
        assert!(validate_key_bits(1024).is_ok(), "vanilla's own key size");
        assert!(validate_key_bits(MIN_KEY_BITS - 1).is_err());
        assert!(validate_key_bits(MAX_KEY_BITS + 1).is_err());
        assert!(validate_key_bits(0).is_err());
    }

    #[test]
    fn undersized_server_key_is_rejected_before_encrypting() {
        // A key well below the accepted range: a malicious/broken server
        // must not get as far as a real encryption attempt against it.
        let (public, _private) = generate_keypair(384).unwrap();
        let der = public.to_public_key_der().unwrap();
        let err = encrypt_pkcs1v15(der.as_bytes(), &[42u8; 16]).unwrap_err();
        assert!(
            matches!(err, ProtocolError::Crypto(msg) if msg.contains("outside accepted range"))
        );
    }
}
