//! Shared types and protocol contracts for the memory server workspace.

pub mod config;
pub mod error;
#[cfg(feature = "http-client")]
pub mod http_client;
pub mod model;
pub mod protocol;
pub mod transcript;
