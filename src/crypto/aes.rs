//! AES-128-CFB8 stream cipher (key = IV = shared secret).
//!
//! Minecraft encrypts the entire byte stream after the Encryption Response
//! with AES-128 in CFB8 mode, using the 16-byte shared secret as both the
//! key and the initialization vector.

use cfb8::cipher::generic_array::GenericArray;
use cfb8::cipher::{BlockDecryptMut, BlockEncryptMut, KeyIvInit};

type Encryptor = cfb8::Encryptor<aes::Aes128>;
type Decryptor = cfb8::Decryptor<aes::Aes128>;

/// Bidirectional AES-128-CFB8 stream cipher for one connection.
///
/// Both directions keep their own keystream state, so the encrypt and
/// decrypt halves of the same instance are independent.
///
/// The cipher-0.4 `AsyncStreamCipher` trait consumes `self`, which would
/// discard the updated CFB8 state. CFB8 uses a one-byte block size, so we
/// instead drive `BlockEncryptMut`/`BlockDecryptMut` per byte, keeping the
/// state inside the struct across calls.
pub struct StreamCipher {
    enc: Encryptor,
    dec: Decryptor,
}

impl StreamCipher {
    /// Creates a cipher from the 16-byte shared secret (key = IV = secret,
    /// Minecraft behavior).
    pub fn new(shared_secret: &[u8; 16]) -> Self {
        Self {
            enc: Encryptor::new(shared_secret.into(), shared_secret.into()),
            dec: Decryptor::new(shared_secret.into(), shared_secret.into()),
        }
    }

    /// Encrypts `data` in place.
    pub fn encrypt(&mut self, data: &mut [u8]) {
        for byte in data.iter_mut() {
            self.enc.encrypt_block_mut(GenericArray::from_mut_slice(std::slice::from_mut(byte)));
        }
    }

    /// Decrypts `data` in place.
    pub fn decrypt(&mut self, data: &mut [u8]) {
        for byte in data.iter_mut() {
            self.dec.decrypt_block_mut(GenericArray::from_mut_slice(std::slice::from_mut(byte)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let secret = [7u8; 16];
        let mut cipher = StreamCipher::new(&secret);
        let original = b"the quick brown fox".to_vec();
        let mut data = original.clone();
        cipher.encrypt(&mut data);
        assert_ne!(data, original);
        cipher.decrypt(&mut data);
        assert_eq!(data, original);
    }

    #[test]
    fn two_instances_interoperate() {
        let secret = [0x42u8; 16];
        let mut a = StreamCipher::new(&secret);
        let mut b = StreamCipher::new(&secret);
        let original = b"0123456789abcdef0123456789abcdef".to_vec();
        let mut data = original.clone();
        a.encrypt(&mut data);
        b.decrypt(&mut data);
        assert_eq!(data, original);
    }

    #[test]
    fn streaming_state_matches_across_chunks() {
        let secret = [3u8; 16];
        let mut one_shot = StreamCipher::new(&secret);
        let mut chunked = StreamCipher::new(&secret);

        let mut big = vec![0x5Au8; 100];
        one_shot.encrypt(&mut big);

        let mut a = vec![0x5Au8; 30];
        let mut b = vec![0x5Au8; 70];
        chunked.encrypt(&mut a);
        chunked.encrypt(&mut b);

        assert_eq!(&big[..30], &a[..]);
        assert_eq!(&big[30..], &b[..]);
    }

    #[test]
    fn streaming_interop_across_instances() {
        // One instance encrypts in two chunks; another decrypts in three
        // differently-sized chunks. CFB8 state must line up regardless.
        let secret = [9u8; 16];
        let mut enc = StreamCipher::new(&secret);
        let mut dec = StreamCipher::new(&secret);

        let mut c1 = vec![0x11u8; 50];
        let mut c2 = vec![0x22u8; 17];
        let p1 = c1.clone();
        let p2 = c2.clone();
        enc.encrypt(&mut c1);
        enc.encrypt(&mut c2);

        let mut stream = [c1, c2].concat();
        dec.decrypt(&mut stream[..10]);
        dec.decrypt(&mut stream[10..60]);
        dec.decrypt(&mut stream[60..]);
        assert_eq!(stream, [p1, p2].concat());
    }
}
