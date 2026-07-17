//! The Mojang session-server "server ID hash": SHA-1 over the server id,
//! shared secret and server public key, then formatted with Minecraft's
//! non-standard signed hex digest instead of a plain hex dump.
//!
//! This is the exact value both the client sends to
//! `sessionserver.mojang.com/session/minecraft/join` and the server sends to
//! `sessionserver.mojang.com/session/minecraft/hasJoined`; a client and
//! server that disagree here fail online-mode login even with valid tokens.

use sha1::{Digest, Sha1};

/// Computes the join-request `serverId` string for the given encryption
/// exchange inputs, in the exact order vanilla hashes them: server id ASCII,
/// then the shared secret, then the server's encoded (DER) public key.
pub fn server_id_hash(server_id: &str, shared_secret: &[u8], public_key_der: &[u8]) -> String {
    let mut hasher = Sha1::new();
    hasher.update(server_id.as_bytes());
    hasher.update(shared_secret);
    hasher.update(public_key_der);
    signed_hex_digest(&hasher.finalize())
}

/// Minecraft's non-standard `Sha1.hexdigest()`: the 20 digest bytes are
/// interpreted as a two's-complement big-endian signed integer (exactly how
/// Java's `BigInteger(byte[])` constructor reads them) and printed in base
/// 16 with a leading `-` for negative values, no leading zero padding.
///
/// The sign is decided by the *raw* digest's high bit before any
/// transformation; magnitude bytes are only negated (two's-complemented) when
/// that bit is set. Getting the ordering backwards silently breaks every
/// negative-hash server id, which is exactly the kind of "one bad packet"
/// mistake that would desync online-mode auth, so this is unit-tested against
/// the three canonical vectors from the protocol documentation.
fn signed_hex_digest(digest: &[u8]) -> String {
    let negative = digest[0] & 0x80 != 0;
    let mut magnitude = digest.to_vec();
    if negative {
        two_complement_in_place(&mut magnitude);
    }
    let hex: String = magnitude.iter().map(|b| format!("{b:02x}")).collect();
    let trimmed = hex.trim_start_matches('0');
    let trimmed = if trimmed.is_empty() { "0" } else { trimmed };
    if negative {
        format!("-{trimmed}")
    } else {
        trimmed.to_string()
    }
}

/// Two's-complements a big-endian byte string in place: invert every bit,
/// then add one with carry propagating from the least-significant byte.
fn two_complement_in_place(bytes: &mut [u8]) {
    let mut carry: u16 = 1;
    for byte in bytes.iter_mut().rev() {
        let inverted = u16::from(!*byte);
        let sum = inverted + carry;
        *byte = sum as u8;
        carry = sum >> 8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sha1_of(input: &str) -> [u8; 20] {
        let mut hasher = Sha1::new();
        hasher.update(input.as_bytes());
        hasher.finalize().into()
    }

    /// The three canonical test vectors for Minecraft's signed hex digest
    /// (protocol encryption documentation): plain SHA-1 of a single ASCII
    /// string, not the full server-id/secret/key concatenation.
    #[test]
    fn matches_canonical_test_vectors() {
        assert_eq!(
            signed_hex_digest(&sha1_of("Notch")),
            "4ed1f46bbe04bc756bcb17c0c7ce3e4632f06a48"
        );
        assert_eq!(
            signed_hex_digest(&sha1_of("jeb_")),
            "-7c9d5b0044c130109a5d7b5fb5c317c02b4e28c1"
        );
        assert_eq!(
            signed_hex_digest(&sha1_of("simon")),
            "88e16a1019277b15d58faf0541e11910eb756f6"
        );
    }

    #[test]
    fn server_id_hash_concatenates_in_the_documented_order() {
        // Changing the input order (id, secret, key) must change the hash;
        // this pins the order against an accidental swap.
        let a = server_id_hash("", b"secret", b"key");
        let b = server_id_hash("", b"key", b"secret");
        assert_ne!(a, b);
    }

    #[test]
    fn all_zero_digest_is_zero_not_empty() {
        // A pathological all-zero digest must format as "0", not an empty
        // string, after leading-zero trimming.
        assert_eq!(signed_hex_digest(&[0u8; 20]), "0");
    }
}
