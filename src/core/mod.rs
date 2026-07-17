//! Engine core: client orchestration, connection state machine, tick loop,
//! and the shared error type.

pub mod client;
pub mod error;
pub mod state;
pub mod supervisor;
pub mod tick;
