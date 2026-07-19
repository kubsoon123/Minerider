//! Vanilla-compatible server resource-pack acknowledgement.
//!
//! MineRider is headless, so textures are not rendered. It still downloads
//! the offered ZIP, enforces a size/time bound, verifies the optional SHA-1,
//! and reports the same state sequence as the vanilla client. This is enough
//! for servers which require a valid resource-pack handshake before joining.

use std::time::Duration;

use minerider_protocol::buffer::PacketWriter;
use minerider_protocol::generated::v1_21_4::types::{
    PacketCommonAddResourcePack, PacketResourcePackReceive,
};
use minerider_protocol::traits::Encode;
use sha1::{Digest, Sha1};
use tracing::{debug, info, warn};

use crate::core::error::Result;
use crate::network::connection::Connection;

pub const STATUS_SUCCESSFULLY_LOADED: i32 = 0;
pub const STATUS_DECLINED: i32 = 1;
pub const STATUS_FAILED_DOWNLOAD: i32 = 2;
pub const STATUS_ACCEPTED: i32 = 3;
pub const STATUS_DOWNLOADED: i32 = 4;
pub const STATUS_INVALID_URL: i32 = 5;

const MAX_RESOURCE_PACK_BYTES: u64 = 100 * 1024 * 1024;
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug)]
enum DownloadFailure {
    InvalidUrl,
    Failed(&'static str),
}

/// Handles one add_resource_pack packet without turning a download failure
/// into a broken Minecraft connection. The server receives the protocol
/// status and can decide whether a forced pack should cause a disconnect.
pub async fn handle_offer(
    conn: &mut Connection,
    response_packet_id: i32,
    pack: PacketCommonAddResourcePack,
    accept_resource_packs: bool,
) -> Result<()> {
    if !accept_resource_packs {
        debug!(uuid = %pack.uuid, forced = pack.forced, "declining resource pack by client setting");
        return send_status(conn, response_packet_id, pack.uuid, STATUS_DECLINED).await;
    }

    send_status(conn, response_packet_id, pack.uuid, STATUS_ACCEPTED).await?;
    let download = tokio::time::timeout(DOWNLOAD_TIMEOUT, download_and_verify(&pack)).await;

    match download {
        Ok(Ok(size)) => {
            send_status(conn, response_packet_id, pack.uuid, STATUS_DOWNLOADED).await?;
            send_status(
                conn,
                response_packet_id,
                pack.uuid,
                STATUS_SUCCESSFULLY_LOADED,
            )
            .await?;
            info!(uuid = %pack.uuid, bytes = size, "resource pack downloaded and accepted");
        }
        Ok(Err(DownloadFailure::InvalidUrl)) => {
            warn!(uuid = %pack.uuid, "resource pack rejected: invalid URL");
            send_status(conn, response_packet_id, pack.uuid, STATUS_INVALID_URL).await?;
        }
        Ok(Err(DownloadFailure::Failed(reason))) => {
            warn!(uuid = %pack.uuid, reason, "resource pack download failed");
            send_status(conn, response_packet_id, pack.uuid, STATUS_FAILED_DOWNLOAD).await?;
        }
        Err(_) => {
            warn!(uuid = %pack.uuid, "resource pack download timed out");
            send_status(conn, response_packet_id, pack.uuid, STATUS_FAILED_DOWNLOAD).await?;
        }
    }
    Ok(())
}

async fn send_status(
    conn: &mut Connection,
    packet_id: i32,
    uuid: u128,
    result: i32,
) -> Result<()> {
    let mut writer = PacketWriter::new();
    PacketResourcePackReceive { uuid, result }.encode(&mut writer)?;
    conn.send_packet(packet_id, &writer.freeze()).await
}

async fn download_and_verify(
    pack: &PacketCommonAddResourcePack,
) -> std::result::Result<u64, DownloadFailure> {
    let url = reqwest::Url::parse(&pack.url).map_err(|_| DownloadFailure::InvalidUrl)?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(DownloadFailure::InvalidUrl);
    }

    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .map_err(|_| DownloadFailure::Failed("HTTP client initialization"))?;
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|_| DownloadFailure::Failed("HTTP request"))?;
    if !response.status().is_success() {
        return Err(DownloadFailure::Failed("HTTP status"));
    }
    let declared_size = response
        .content_length()
        .ok_or(DownloadFailure::Failed("missing Content-Length"))?;
    if declared_size > MAX_RESOURCE_PACK_BYTES {
        return Err(DownloadFailure::Failed("pack exceeds 100 MiB limit"));
    }

    let bytes = response
        .bytes()
        .await
        .map_err(|_| DownloadFailure::Failed("response body"))?;
    let size = bytes.len() as u64;
    if size > MAX_RESOURCE_PACK_BYTES || size != declared_size {
        return Err(DownloadFailure::Failed("invalid response size"));
    }
    let mut sha1 = Sha1::new();
    sha1.update(&bytes);

    let expected = pack.hash.trim();
    if !expected.is_empty() {
        if expected.len() != 40 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(DownloadFailure::Failed("invalid SHA-1 field"));
        }
        let actual = format!("{:x}", sha1.finalize());
        if !actual.eq_ignore_ascii_case(expected) {
            return Err(DownloadFailure::Failed("SHA-1 mismatch"));
        }
    }

    Ok(size)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn rejects_non_http_urls_as_invalid() {
        let pack = PacketCommonAddResourcePack {
            uuid: 1,
            url: "file:///tmp/pack.zip".to_string(),
            hash: String::new(),
            forced: false,
            prompt_message: None,
        };
        assert!(matches!(
            download_and_verify(&pack).await,
            Err(DownloadFailure::InvalidUrl)
        ));
    }

    #[tokio::test]
    async fn downloads_and_verifies_sha1() {
        let body = b"PK\x03\x04fake-resource-pack";
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request).await.unwrap();
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(headers.as_bytes()).await.unwrap();
            stream.write_all(body).await.unwrap();
        });
        let hash = format!("{:x}", Sha1::digest(body));
        let pack = PacketCommonAddResourcePack {
            uuid: 2,
            url: format!("http://{address}/pack.zip"),
            hash,
            forced: true,
            prompt_message: None,
        };

        assert_eq!(
            download_and_verify(&pack).await.unwrap(),
            body.len() as u64
        );
        server.await.unwrap();
    }
}
