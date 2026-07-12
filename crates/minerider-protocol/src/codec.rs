//! Frame codec: length-prefix framing plus optional zlib compression.
//!
//! Wire format without compression:
//!
//! ```text
//! VarInt frame_len | VarInt packet_id | payload
//! ```
//!
//! Wire format with compression enabled:
//!
//! ```text
//! VarInt frame_len | VarInt data_length | body
//! ```
//!
//! where `body` is the zlib-compressed `packet_id + payload` when
//! `data_length > 0` (its uncompressed size), or the raw bytes when
//! `data_length == 0`.

use std::borrow::Cow;

use bytes::{Buf, BytesMut};

use super::buffer::PacketReader;
use super::compression;
use super::packet::RawPacket;
use super::varint;
use crate::error::{ProtocolError, Result};

/// Maximum frame size accepted on the wire (2 MiB).
pub const MAX_FRAME_SIZE: usize = 2 * 1024 * 1024;

/// Compression-aware packet framing codec.
///
/// Holds the compression threshold for one direction of the connection.
/// `None` or a negative threshold disables compression.
#[derive(Debug, Default)]
pub struct FrameCodec {
    compression_threshold: Option<i32>,
}

impl FrameCodec {
    /// Creates a codec with compression disabled.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the compression threshold. Negative values disable compression.
    pub fn set_compression_threshold(&mut self, threshold: i32) {
        self.compression_threshold = if threshold < 0 { None } else { Some(threshold) };
    }

    /// Current compression threshold, if any.
    pub fn compression_threshold(&self) -> Option<i32> {
        self.compression_threshold
    }

    /// Encodes a packet into a complete wire frame.
    pub fn encode(&self, packet: &RawPacket) -> Result<BytesMut> {
        // body = VarInt(id) + payload
        let mut body = BytesMut::with_capacity(packet.payload.len() + varint::MAX_VARINT_BYTES);
        varint::write_varint(&mut body, packet.id);
        body.extend_from_slice(&packet.payload);

        let framed_body = match self.compression_threshold {
            Some(threshold) if body.len() >= threshold as usize => {
                let compressed = compression::compress(&body)?;
                let mut out = BytesMut::with_capacity(compressed.len() + varint::MAX_VARINT_BYTES);
                varint::write_varint(&mut out, body.len() as i32);
                out.extend_from_slice(&compressed);
                out
            }
            Some(_) => {
                let mut out = BytesMut::with_capacity(body.len() + 1);
                varint::write_varint(&mut out, 0);
                out.extend_from_slice(&body);
                out
            }
            None => body,
        };

        let mut frame = BytesMut::with_capacity(framed_body.len() + varint::MAX_VARINT_BYTES);
        varint::write_varint(&mut frame, framed_body.len() as i32);
        frame.extend_from_slice(&framed_body);
        Ok(frame)
    }

    /// Attempts to decode one frame from the front of `buf`.
    ///
    /// Returns `Ok(None)` if a complete frame is not yet buffered; in that
    /// case `buf` is left untouched. On success the frame bytes are consumed
    /// from `buf`. Malformed frames (oversized, corrupt VarInt lengths,
    /// decompression failures) yield a [`ProtocolError`].
    pub fn try_decode(&self, buf: &mut BytesMut) -> Result<Option<RawPacket>> {
        // Peek the frame-length VarInt without consuming bytes.
        let Some((frame_len, varint_len)) = peek_frame_length(buf)? else {
            return Ok(None);
        };
        if buf.len() - varint_len < frame_len {
            return Ok(None);
        }

        // Full frame available: consume it.
        buf.advance(varint_len);
        let frame_bytes = buf.split_to(frame_len);

        // Borrow the frame bytes when no decompression is needed so the
        // uncompressed path performs a single copy (into the payload).
        let body: Cow<'_, [u8]> = match self.compression_threshold {
            Some(_) => {
                let mut r = PacketReader::new(&frame_bytes);
                let data_length = r.get_varint()?;
                if data_length < 0 {
                    return Err(ProtocolError::NegativeLength(data_length));
                }
                // A ≤ 2 MiB frame must not declare an inflated body beyond
                // the packet cap; compression.rs's own 64 MiB cap stays as
                // defense in depth.
                if data_length > MAX_FRAME_SIZE as i32 {
                    return Err(ProtocolError::FrameTooLarge {
                        length: data_length as usize,
                        max: MAX_FRAME_SIZE,
                    });
                }
                if data_length == 0 {
                    Cow::Borrowed(r.rest())
                } else {
                    Cow::Owned(compression::decompress(r.rest(), data_length as usize)?)
                }
            }
            None => Cow::Borrowed(&frame_bytes[..]),
        };

        // Split the first VarInt of the body as the packet id.
        let mut r = PacketReader::new(&body);
        let id = r.get_varint()?;
        let payload = BytesMut::from(r.rest());
        Ok(Some(RawPacket::new(id, payload)))
    }
}

/// Reads the frame-length VarInt from the front of `buf` without consuming
/// it. Returns `(frame_len, varint_byte_count)`, `None` if the VarInt is not
/// complete yet, or an error if the VarInt is malformed or the declared
/// frame exceeds [`MAX_FRAME_SIZE`].
fn peek_frame_length(buf: &BytesMut) -> Result<Option<(usize, usize)>> {
    let mut result: u32 = 0;
    for i in 0..varint::MAX_VARINT_BYTES {
        let Some(&byte) = buf.get(i) else {
            return Ok(None);
        };
        result |= ((byte & 0x7F) as u32) << (i * 7);
        if byte & 0x80 == 0 {
            let frame_len = result as usize;
            if frame_len > MAX_FRAME_SIZE {
                return Err(ProtocolError::FrameTooLarge {
                    length: frame_len,
                    max: MAX_FRAME_SIZE,
                });
            }
            return Ok(Some((frame_len, i + 1)));
        }
    }
    Err(ProtocolError::VarIntTooLong)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::PacketWriter;

    fn packet(id: i32, payload: &[u8]) -> RawPacket {
        RawPacket::new(id, BytesMut::from(payload))
    }

    #[test]
    fn roundtrip_without_compression() {
        let codec = FrameCodec::new();
        let p = packet(0x2A, b"hello world");
        let frame = codec.encode(&p).unwrap();
        let mut buf = frame;
        let out = codec.try_decode(&mut buf).unwrap().unwrap();
        assert_eq!(out.id, 0x2A);
        assert_eq!(&out.payload[..], b"hello world");
        assert!(buf.is_empty());
    }

    #[test]
    fn roundtrip_small_packet_uncompressed_path() {
        let mut codec = FrameCodec::new();
        codec.set_compression_threshold(64);
        let p = packet(0x01, b"small");
        let frame = codec.encode(&p).unwrap();
        // Frame: [len][data_length=0][id][payload]
        assert_eq!(frame[1], 0x00, "data_length should be 0 for small packets");
        let mut buf = frame;
        let out = codec.try_decode(&mut buf).unwrap().unwrap();
        assert_eq!(out.id, 0x01);
        assert_eq!(&out.payload[..], b"small");
    }

    #[test]
    fn roundtrip_large_packet_compressed_path() {
        let mut codec = FrameCodec::new();
        codec.set_compression_threshold(64);
        let payload = vec![0xAB; 1024];
        let p = packet(0x07, &payload);
        let frame = codec.encode(&p).unwrap();
        // data_length VarInt must be > 0 (compressed path).
        assert_ne!(frame[1], 0x00, "large packet must be compressed");
        let mut buf = frame;
        let out = codec.try_decode(&mut buf).unwrap().unwrap();
        assert_eq!(out.id, 0x07);
        assert_eq!(&out.payload[..], &payload[..]);
    }

    #[test]
    fn compression_threshold_boundary() {
        // The threshold compares against body length = id VarInt + payload.
        // With a one-byte id: body == threshold - 1 stays uncompressed,
        // body == threshold is compressed.
        let mut codec = FrameCodec::new();
        codec.set_compression_threshold(6);

        let under = codec.encode(&packet(0x01, &[0xAA; 4])).unwrap();
        assert_eq!(under[1], 0x00, "body len 5 = threshold-1 must stay raw");
        let mut buf = under;
        let out = codec.try_decode(&mut buf).unwrap().unwrap();
        assert_eq!(&out.payload[..], &[0xAA; 4]);

        let at = codec.encode(&packet(0x01, &[0xBB; 5])).unwrap();
        assert_ne!(at[1], 0x00, "body len 6 = threshold must be compressed");
        let mut buf = at;
        let out = codec.try_decode(&mut buf).unwrap().unwrap();
        assert_eq!(&out.payload[..], &[0xBB; 5]);
    }

    #[test]
    fn corrupted_compressed_length_errors() {
        let mut codec = FrameCodec::new();
        codec.set_compression_threshold(64);
        let p = packet(0x07, &vec![0xAB; 1024]);
        let mut frame = codec.encode(&p).unwrap();
        // Overwrite data_length with a wrong (smaller) value.
        frame[1] = 0x20;
        let mut buf = frame;
        assert!(matches!(
            codec.try_decode(&mut buf),
            Err(ProtocolError::Compression(_))
        ));
    }

    #[test]
    fn negative_data_length_errors() {
        let mut codec = FrameCodec::new();
        codec.set_compression_threshold(64);
        // Frame: len=2, data_length=-1 (0xFF 0xFF 0xFF 0xFF 0x0F).
        let mut buf = BytesMut::from(&[6, 0xFF, 0xFF, 0xFF, 0xFF, 0x0F, 0x00][..]);
        assert!(matches!(
            codec.try_decode(&mut buf),
            Err(ProtocolError::NegativeLength(-1))
        ));
    }

    #[test]
    fn oversized_data_length_errors() {
        let mut codec = FrameCodec::new();
        codec.set_compression_threshold(64);
        // Frame within the 2 MiB frame cap, but data_length claims the body
        // inflates to i32::MAX (0xFF 0xFF 0xFF 0xFF 0x07): must be rejected
        // before any decompression is attempted.
        let mut buf = BytesMut::from(&[6, 0xFF, 0xFF, 0xFF, 0xFF, 0x07, 0x00][..]);
        assert!(matches!(
            codec.try_decode(&mut buf),
            Err(ProtocolError::FrameTooLarge { .. })
        ));
    }

    #[test]
    fn malformed_packet_id_varint_errors() {
        let codec = FrameCodec::new();
        // Frame len 6, body = six continuation bytes: not a valid id VarInt.
        let mut buf = BytesMut::from(&[6, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80][..]);
        assert!(matches!(
            codec.try_decode(&mut buf),
            Err(ProtocolError::VarIntTooLong)
        ));
    }

    #[test]
    fn incomplete_frame_returns_none_and_preserves_buffer() {
        let codec = FrameCodec::new();
        let p = packet(0x2A, b"hello world");
        let frame = codec.encode(&p).unwrap();
        // Feed only part of the frame.
        let mut buf = frame.clone();
        buf.truncate(frame.len() - 2);
        assert!(codec.try_decode(&mut buf).unwrap().is_none());
        assert_eq!(buf.len(), frame.len() - 2);
        // Also test an incomplete length VarInt (0x80 sets continuation bit).
        let mut partial_len = BytesMut::from(&[0x80][..]);
        assert!(codec.try_decode(&mut partial_len).unwrap().is_none());
        assert_eq!(partial_len.len(), 1);
        // Now complete the frame and decode.
        let mut full = frame;
        assert!(codec.try_decode(&mut full).unwrap().is_some());
    }

    #[test]
    fn oversized_frame_errors() {
        let codec = FrameCodec::new();
        // VarInt for 3 MiB: 3*1024*1024 = 0x300000 → 0x80 0x80 0xC0 0x01
        let mut buf = BytesMut::new();
        buf.extend_from_slice(&[0x80, 0x80, 0xC0, 0x01]);
        buf.extend_from_slice(&[0u8; 16]);
        assert!(matches!(
            codec.try_decode(&mut buf),
            Err(ProtocolError::FrameTooLarge { .. })
        ));
    }

    #[test]
    fn malformed_length_varint_errors() {
        let codec = FrameCodec::new();
        let mut buf = BytesMut::from(&[0x80, 0x80, 0x80, 0x80, 0x80, 0x01][..]);
        assert!(matches!(
            codec.try_decode(&mut buf),
            Err(ProtocolError::VarIntTooLong)
        ));
    }

    #[test]
    fn back_to_back_frames_decode() {
        let codec = FrameCodec::new();
        let mut buf = BytesMut::new();
        buf.extend_from_slice(&codec.encode(&packet(1, b"a")).unwrap());
        buf.extend_from_slice(&codec.encode(&packet(2, b"bb")).unwrap());
        let first = codec.try_decode(&mut buf).unwrap().unwrap();
        let second = codec.try_decode(&mut buf).unwrap().unwrap();
        assert_eq!((first.id, &first.payload[..]), (1, &b"a"[..]));
        assert_eq!((second.id, &second.payload[..]), (2, &b"bb"[..]));
        assert!(codec.try_decode(&mut buf).unwrap().is_none());
    }

    #[test]
    fn negative_threshold_disables_compression() {
        let mut codec = FrameCodec::new();
        codec.set_compression_threshold(-1);
        assert_eq!(codec.compression_threshold(), None);
        let p = packet(0x01, &vec![0xAB; 1024]);
        let frame = codec.encode(&p).unwrap();
        let mut w = PacketWriter::new();
        w.put_varint(p.id);
        w.put_bytes(&p.payload);
        let body = w.into_inner();
        assert_eq!(frame.len(), body.len() + 2);
    }
}
