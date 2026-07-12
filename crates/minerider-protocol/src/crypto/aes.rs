//! AES-128-CFB8 stream cipher (key = IV = shared secret).
//!
//! Minecraft encrypts the entire byte stream after the Encryption Response
//! with AES-128 in CFB8 mode, using the 16-byte shared secret as both the
//! key and the initialization vector.
//!
//! Implementation note: CFB8 with a 16-byte IV shift register costs one AES
//! block encryption per byte by construction — what matters is the per-byte
//! overhead. cipher-0.4's `AsyncStreamCipher::encrypt` consumes `self`
//! (which would drop the updated CFB8 state), and driving
//! `BlockEncryptMut::encrypt_block_mut` per byte re-dispatches the AES
//! backend through the generic closure machinery on every single byte. We
//! therefore run the CFB8 feedback loop directly on `Aes128Enc` (the
//! RustCrypto AES block primitive — only the ~10 lines of mode wiring are
//! local): one `encrypt_block` plus a `copy_within` register shift per
//! byte, as a single bulk call that keeps the shift registers inside the
//! struct across calls. Correctness is pinned by a known-answer vector
//! produced with `openssl enc -aes-128-cfb8`.

use aes::cipher::{generic_array::GenericArray, BlockEncrypt, KeyInit};

/// Bidirectional AES-128-CFB8 stream cipher for one connection.
///
/// Both directions keep their own IV shift register, so the encrypt and
/// decrypt halves of the same instance are independent. One `Aes128Enc`
/// serves both directions: CFB mode only ever encrypts with the block
/// cipher, even when decrypting the stream.
pub struct StreamCipher {
    aes: aes::Aes128Enc,
    enc_iv: [u8; 16],
    dec_iv: [u8; 16],
}

impl StreamCipher {
    /// Creates a cipher from the 16-byte shared secret (key = IV = secret,
    /// Minecraft behavior).
    pub fn new(shared_secret: &[u8; 16]) -> Self {
        Self {
            aes: aes::Aes128Enc::new(shared_secret.into()),
            enc_iv: *shared_secret,
            dec_iv: *shared_secret,
        }
    }

    /// Encrypts `data` in place, advancing the CFB8 state.
    pub fn encrypt(&mut self, data: &mut [u8]) {
        let iv = &mut self.enc_iv;
        for byte in data.iter_mut() {
            let mut t = GenericArray::clone_from_slice(&iv[..]);
            self.aes.encrypt_block(&mut t);
            *byte ^= t[0];
            iv.copy_within(1.., 0);
            iv[15] = *byte;
        }
    }

    /// Decrypts `data` in place, advancing the CFB8 state.
    pub fn decrypt(&mut self, data: &mut [u8]) {
        let iv = &mut self.dec_iv;
        for byte in data.iter_mut() {
            let mut t = GenericArray::clone_from_slice(&iv[..]);
            self.aes.encrypt_block(&mut t);
            let c = *byte;
            *byte ^= t[0];
            iv.copy_within(1.., 0);
            iv[15] = c;
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
    fn known_answer_vector() {
        // AES-128-CFB8, key = IV = 00..0f, plaintext 00..20.
        // Expected ciphertext produced by `openssl enc -aes-128-cfb8`
        // (independent of this implementation).
        let secret: [u8; 16] = std::array::from_fn(|i| i as u8);
        let plaintext: Vec<u8> = (0u8..=0x20).collect();
        let expected: &[u8] = &[
            0x0a, 0x22, 0xf7, 0x96, 0xe1, 0xb9, 0x3e, 0x90, 0x32, 0xcf, 0xf8, 0x04, 0x83, 0x8a,
            0xdf, 0xc3, 0xa5, 0xe4, 0xb3, 0xff, 0xdd, 0x47, 0x10, 0x85, 0x75, 0x53, 0x3e, 0x67,
            0x2e, 0xf5, 0xd8, 0xef, 0x49,
        ];

        let mut cipher = StreamCipher::new(&secret);
        let mut data = plaintext.clone();
        cipher.encrypt(&mut data);
        assert_eq!(data, expected, "encryption must match the known vector");

        let mut cipher = StreamCipher::new(&secret);
        cipher.decrypt(&mut data);
        assert_eq!(data, plaintext, "decryption must invert the vector");
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
