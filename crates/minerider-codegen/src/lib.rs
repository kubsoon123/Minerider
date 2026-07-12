//! minecraft-data driven packet definition generator.
//!
//! Reads a vendored minecraft-data `protocol.json` + `version.json` pair,
//! validates it, resolves it into an intermediate representation, and emits
//! Rust packet definitions into `minerider-protocol/src/generated/`.
//!
//! Pipeline: vendored data → [`model`] (serde) → [`parse`] (load) →
//! [`ir`] (resolve + validate) → [`emit`] (Rust source).

#![forbid(unsafe_code)]

pub mod emit;
pub mod ir;
pub mod model;
pub mod parse;

pub use ir::{Ir, State};
pub use parse::{load, CodegenError, Result};
