//! Tokio TCP transport: connect helper producing split read/write halves.

use std::time::Duration;

use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;

use crate::core::error::{MineRiderError, Result};

/// A connected TCP stream, split into owned read and write halves.
pub struct TcpTransport {
    /// Read half of the connected stream.
    pub read: OwnedReadHalf,
    /// Write half of the connected stream.
    pub write: OwnedWriteHalf,
}

impl TcpTransport {
    /// Connects to `host:port`, failing with [`MineRiderError::Timeout`] if
    /// the connection cannot be established within `timeout`.
    pub async fn connect(host: &str, port: u16, timeout: Duration) -> Result<TcpTransport> {
        let stream = tokio::time::timeout(timeout, TcpStream::connect((host, port)))
            .await
            .map_err(|_| MineRiderError::Timeout(format!("connecting to {host}:{port}")))??;
        stream.set_nodelay(true)?;
        let (read, write) = stream.into_split();
        Ok(TcpTransport { read, write })
    }
}
