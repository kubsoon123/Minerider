//! Loading and version validation of vendored minecraft-data files.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::model::ProtocolFile;

/// Protocol number this generator supports.
pub const SUPPORTED_PROTOCOL: i32 = 769;
/// Minecraft version this generator supports.
pub const SUPPORTED_MINECRAFT: &str = "1.21.4";

/// Every fallible operation in the codegen crate returns this error.
#[derive(Debug, thiserror::Error)]
pub enum CodegenError {
    /// A file could not be read.
    #[error("cannot read {path}: {source}")]
    Io {
        /// File that failed.
        path: PathBuf,
        /// Underlying I/O error.
        source: std::io::Error,
    },

    /// A JSON file could not be parsed or did not match the model.
    #[error("cannot parse {path}: {source}")]
    Json {
        /// File that failed.
        path: PathBuf,
        /// Underlying serde error.
        source: serde_json::Error,
    },

    /// `version.json` does not describe the supported version.
    #[error(
        "unsupported version in {path}: protocol {protocol}, minecraft {minecraft:?} \
         (expected protocol {expected_protocol}, minecraft {expected_minecraft:?})"
    )]
    VersionMismatch {
        /// File that failed.
        path: PathBuf,
        /// Found protocol number.
        protocol: i32,
        /// Found Minecraft version.
        minecraft: String,
        /// Supported protocol number.
        expected_protocol: i32,
        /// Supported Minecraft version.
        expected_minecraft: &'static str,
    },

    /// A semantic problem in the protocol data (unresolved reference,
    /// inconsistent packet table, unsupported construct). Always names the
    /// packet/field/type involved.
    #[error("invalid protocol data: {0}")]
    Invalid(String),
}

/// Convenience alias for codegen results.
pub type Result<T> = std::result::Result<T, CodegenError>;

/// A parsed `version.json`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct VersionJson {
    /// Human Minecraft version, e.g. `"1.21.4"`.
    #[serde(rename = "minecraftVersion")]
    pub minecraft_version: String,
    /// Protocol number, e.g. `769`.
    pub version: i32,
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let text = std::fs::read_to_string(path).map_err(|source| CodegenError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    serde_json::from_str(&text).map_err(|source| CodegenError::Json {
        path: path.to_path_buf(),
        source,
    })
}

/// Loads `version.json` and `protocol.json` from `dir` and validates that
/// they describe the supported Minecraft version.
pub fn load(dir: &Path) -> Result<(VersionJson, ProtocolFile)> {
    let version_path = dir.join("version.json");
    let version: VersionJson = read_json(&version_path)?;
    if version.version != SUPPORTED_PROTOCOL || version.minecraft_version != SUPPORTED_MINECRAFT {
        return Err(CodegenError::VersionMismatch {
            path: version_path,
            protocol: version.version,
            minecraft: version.minecraft_version,
            expected_protocol: SUPPORTED_PROTOCOL,
            expected_minecraft: SUPPORTED_MINECRAFT,
        });
    }
    let protocol: ProtocolFile = read_json(&dir.join("protocol.json"))?;
    Ok((version, protocol))
}
