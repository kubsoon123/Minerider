//! Throughput benchmarks for the protocol primitives: VarInt, frame codec
//! (with and without compression) and AES-128-CFB8.
//!
//! Numbers are recorded in `docs/architecture.md`.

use std::time::Duration;

use bytes::BytesMut;
use cfb8::cipher::generic_array::GenericArray;
use cfb8::cipher::{BlockEncryptMut, KeyIvInit};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use minerider_protocol::codec::FrameCodec;
use minerider_protocol::crypto::aes::StreamCipher;
use minerider_protocol::packet::RawPacket;
use minerider_protocol::varint::{read_varint, write_varint};

fn bench_varint(c: &mut Criterion) {
    let mut group = c.benchmark_group("varint");
    // Mixed-size values (1..5 bytes) so the number reflects realistic traffic.
    let values: Vec<i32> = (0..1024)
        .map(|i| match i % 4 {
            0 => i,
            1 => i * 16384,
            2 => i * 2_097_152,
            _ => -(i * 268_435_456),
        })
        .collect();
    let encoded_len: usize = values
        .iter()
        .map(|&v| minerider_protocol::varint::varint_size(v))
        .sum();
    group.throughput(Throughput::Bytes(encoded_len as u64));

    group.bench_function("encode_1024", |b| {
        let mut buf = BytesMut::with_capacity(encoded_len);
        b.iter(|| {
            buf.clear();
            for &v in &values {
                write_varint(&mut buf, v);
            }
        });
    });

    let mut buf = BytesMut::new();
    for &v in &values {
        write_varint(&mut buf, v);
    }
    let encoded = buf.freeze();
    group.bench_function("decode_1024", |b| {
        b.iter(|| {
            let mut r = minerider_protocol::buffer::PacketReader::new(&encoded);
            for _ in 0..values.len() {
                std::hint::black_box(read_varint(&mut r).unwrap());
            }
        });
    });
    group.finish();
}

fn bench_frame(c: &mut Criterion) {
    let mut group = c.benchmark_group("frame");

    // 1 KiB of repetitive data compresses well; 64 KiB of pseudo-random
    // data barely compresses — the two extremes of the compressed path.
    let repetitive = vec![0xABu8; 1024];
    let mut random = vec![0u8; 64 * 1024];
    let mut state: u32 = 0x1234_5678;
    for byte in &mut random {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        *byte = state as u8;
    }

    for (name, payload) in [("1k_raw", &repetitive), ("64k_raw", &random)] {
        group.throughput(Throughput::Bytes(payload.len() as u64));
        let codec = FrameCodec::new();
        let packet = RawPacket::new(0x2A, BytesMut::from(&payload[..]));
        let frame = codec.encode(&packet).unwrap();

        group.bench_with_input(BenchmarkId::new("encode", name), payload, |b, _| {
            b.iter(|| codec.encode(&packet).unwrap());
        });
        group.bench_with_input(BenchmarkId::new("decode", name), payload, |b, _| {
            b.iter(|| {
                let mut buf = frame.clone();
                codec.try_decode(&mut buf).unwrap().unwrap()
            });
        });
    }

    for (name, payload) in [("1k_compressed", &repetitive), ("64k_compressed", &random)] {
        group.throughput(Throughput::Bytes(payload.len() as u64));
        let mut codec = FrameCodec::new();
        codec.set_compression_threshold(64);
        let packet = RawPacket::new(0x2A, BytesMut::from(&payload[..]));
        let frame = codec.encode(&packet).unwrap();

        group.bench_with_input(BenchmarkId::new("encode", name), payload, |b, _| {
            b.iter(|| codec.encode(&packet).unwrap());
        });
        group.bench_with_input(BenchmarkId::new("decode", name), payload, |b, _| {
            b.iter(|| {
                let mut buf = frame.clone();
                codec.try_decode(&mut buf).unwrap().unwrap()
            });
        });
    }
    group.finish();
}

/// Pre-fix AES path: one `encrypt_block_mut` call per byte, dispatching the
/// AES backend for every single byte. Kept in the bench to quantify the
/// before/after of the bulk `StreamCipher` implementation.
fn encrypt_per_byte(enc: &mut cfb8::Encryptor<aes::Aes128>, data: &mut [u8]) {
    for byte in data.iter_mut() {
        enc.encrypt_block_mut(GenericArray::from_mut_slice(std::slice::from_mut(byte)));
    }
}

fn bench_aes(c: &mut Criterion) {
    let mut group = c.benchmark_group("aes_cfb8");
    let secret = [0x42u8; 16];

    // Bulk path over 1 MiB.
    group.throughput(Throughput::Bytes(1024 * 1024));
    group.bench_function("encrypt_bulk_1mib", |b| {
        let mut cipher = StreamCipher::new(&secret);
        let mut data = vec![0x5Au8; 1024 * 1024];
        b.iter(|| cipher.encrypt(&mut data));
    });
    group.bench_function("decrypt_bulk_1mib", |b| {
        let mut cipher = StreamCipher::new(&secret);
        let mut data = vec![0x5Au8; 1024 * 1024];
        b.iter(|| cipher.decrypt(&mut data));
    });

    // Legacy per-byte path over 64 KiB (1 MiB would dominate the bench time).
    group.throughput(Throughput::Bytes(64 * 1024));
    group.bench_function("encrypt_per_byte_64kib_legacy", |b| {
        let mut enc = cfb8::Encryptor::<aes::Aes128>::new((&secret).into(), (&secret).into());
        let mut data = vec![0x5Au8; 64 * 1024];
        b.iter(|| encrypt_per_byte(&mut enc, &mut data));
    });
    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .sample_size(20)
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(2));
    targets = bench_varint, bench_frame, bench_aes
}
criterion_main!(benches);
