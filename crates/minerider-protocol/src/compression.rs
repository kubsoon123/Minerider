//! zlib deflate/inflate helpers used by the protocol codec.

use std::io::Read;

use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;

use crate::error::{ProtocolError, Result};

/// Hard cap on decompressed size (64 MiB) to guard against zip bombs.
const MAX_DECOMPRESSED: u64 = 64 * 1024 * 1024;

/// Compresses `data` with zlib at the default compression level.
pub fn compress(data: &[u8]) -> Result<Vec<u8>> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    std::io::Write::write_all(&mut encoder, data)
        .map_err(|e| ProtocolError::Compression(format!("zlib deflate failed: {e}")))?;
    encoder
        .finish()
        .map_err(|e| ProtocolError::Compression(format!("zlib deflate finish failed: {e}")))
}

/// Decompresses `data` with zlib, requiring the output to be exactly
/// `expected_len` bytes.
///
/// Reads at most `expected_len + 1` bytes from the stream, which both
/// enforces the exact-length contract and caps memory use against zip bombs
/// (anything beyond the cap is an error).
pub fn decompress(data: &[u8], expected_len: usize) -> Result<Vec<u8>> {
    let cap = (expected_len as u64 + 1).min(MAX_DECOMPRESSED);
    let decoder = ZlibDecoder::new(data);
    let mut out = Vec::with_capacity(expected_len.min(1 << 20));
    decoder
        .take(cap)
        .read_to_end(&mut out)
        .map_err(|e| ProtocolError::Compression(format!("zlib inflate failed: {e}")))?;
    if out.len() != expected_len {
        return Err(ProtocolError::Compression(format!(
            "decompressed length mismatch: expected {expected_len} bytes, got {}",
            out.len()
        )));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        for data in [
            &b""[..],
            b"hello world",
            &vec![0xAB; 4096],
            &(0u32..10_000)
                .flat_map(u32::to_le_bytes)
                .collect::<Vec<u8>>(),
        ] {
            let compressed = compress(data).unwrap();
            let out = decompress(&compressed, data.len()).unwrap();
            assert_eq!(out, data);
        }
    }

    #[test]
    fn expected_length_mismatch_errors() {
        let compressed = compress(b"hello world").unwrap();
        assert!(matches!(
            decompress(&compressed, 5),
            Err(ProtocolError::Compression(_))
        ));
        // Larger than actual also fails (short read).
        assert!(matches!(
            decompress(&compressed, 1000),
            Err(ProtocolError::Compression(_))
        ));
    }

    #[test]
    fn corrupt_data_errors() {
        assert!(matches!(
            decompress(&[0x00, 0x01, 0x02], 10),
            Err(ProtocolError::Compression(_))
        ));
    }
}
