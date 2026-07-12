//! Throughput benchmarks for the protocol primitives: VarInt, frame codec
//! (with and without compression) and AES-128-CFB8.
//!
//! Numbers are recorded in `docs/architecture.md`.

use std::time::Duration;

use bytes::BytesMut;
use cfb8::cipher::generic_array::GenericArray;
use cfb8::cipher::{BlockEncryptMut, KeyIvInit};
use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use minerider_protocol::buffer::{PacketReader, PacketWriter};
use minerider_protocol::codec::FrameCodec;
use minerider_protocol::crypto::aes::StreamCipher;
use minerider_protocol::generated::v1_21_4::{configuration, play, types};
use minerider_protocol::packet::RawPacket;
use minerider_protocol::traits::{Decode, Encode};
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

// ---------------------------------------------------------------------------
// Generated packet codecs: dispatch, decode and encode throughput.
// ---------------------------------------------------------------------------

fn sample_settings() -> types::PacketCommonSettings {
    types::PacketCommonSettings {
        locale: "en_us".to_string(),
        view_distance: 10,
        chat_flags: 0,
        chat_colors: true,
        skin_parts: 0x7F,
        main_hand: 1,
        enable_text_filtering: false,
        enable_server_listing: true,
        particle_status: types::PacketCommonSettingsParticleStatus::All,
    }
}

/// String-dispatch baseline (mineflayer-style): resolve a packet name to its
/// id through an if-chain, as opposed to the generated integer match.
fn name_to_id_baseline(name: &str) -> Option<i32> {
    if name == "cookie_request" {
        Some(0)
    } else if name == "custom_payload" {
        Some(1)
    } else if name == "disconnect" {
        Some(2)
    } else if name == "finish_configuration" {
        Some(3)
    } else if name == "keep_alive" {
        Some(4)
    } else if name == "ping" {
        Some(5)
    } else if name == "reset_chat" {
        Some(6)
    } else if name == "registry_data" {
        Some(7)
    } else if name == "remove_resource_pack" {
        Some(8)
    } else if name == "add_resource_pack" {
        Some(9)
    } else if name == "store_cookie" {
        Some(10)
    } else if name == "transfer" {
        Some(11)
    } else if name == "feature_flags" {
        Some(12)
    } else if name == "tags" {
        Some(13)
    } else if name == "select_known_packs" {
        Some(14)
    } else if name == "custom_report_details" {
        Some(15)
    } else if name == "server_links" {
        Some(16)
    } else {
        None
    }
}

fn bench_generated(c: &mut Criterion) {
    let mut group = c.benchmark_group("generated");

    // Small packet (keep alive, 9 bytes of payload): enum-dispatch decode.
    let mut w = PacketWriter::new();
    configuration::ClientboundConfigurationPacket::KeepAlive(configuration::PacketKeepAlive {
        keep_alive_id: 42,
    })
    .encode(&mut w)
    .unwrap();
    let keep_alive_bytes = w.freeze();
    group.bench_function("decode_small_keep_alive", |b| {
        b.iter(|| {
            let mut r = PacketReader::new(&keep_alive_bytes);
            let id = r.get_varint().unwrap();
            black_box(configuration::ClientboundConfigurationPacket::decode(id, &mut r).unwrap());
        });
    });
    group.bench_function("encode_small_keep_alive", |b| {
        let packet = configuration::ClientboundConfigurationPacket::KeepAlive(
            configuration::PacketKeepAlive { keep_alive_id: 42 },
        );
        b.iter(|| {
            let mut w = PacketWriter::new();
            packet.encode(&mut w).unwrap();
            black_box(w.freeze());
        });
    });

    // Medium packet (client information, 22 bytes) and a play keep alive.
    let mut w = PacketWriter::new();
    sample_settings().encode(&mut w).unwrap();
    let settings_bytes = w.freeze();
    group.bench_function("decode_medium_settings", |b| {
        b.iter(|| {
            let mut r = PacketReader::new(&settings_bytes);
            black_box(types::PacketCommonSettings::decode(&mut r).unwrap());
        });
    });
    group.bench_function("encode_medium_settings", |b| {
        let settings = sample_settings();
        b.iter(|| {
            let mut w = PacketWriter::new();
            settings.encode(&mut w).unwrap();
            black_box(w.freeze());
        });
    });

    // Packet-id dispatch: generated integer match vs string if-chain.
    group.bench_function("dispatch_generated_int_match", |b| {
        b.iter(|| black_box(configuration::clientbound_packet_name(black_box(4))));
    });
    group.bench_function("dispatch_string_if_chain", |b| {
        b.iter(|| black_box(name_to_id_baseline(black_box("keep_alive"))));
    });

    // Play-state dispatch over 131 clientbound ids.
    group.bench_function("dispatch_play_int_match", |b| {
        b.iter(|| black_box(play::clientbound_packet_name(black_box(39))));
    });
    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .sample_size(20)
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(2));
    targets = bench_varint, bench_frame, bench_aes, bench_generated
}
criterion_main!(benches);
