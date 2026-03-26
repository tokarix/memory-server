//! HTTP service and background workers for the memory server.

pub mod api;
pub mod app;
pub mod db;
pub mod dream;
pub mod edges;
pub mod embed;
pub mod expand;
pub mod ollama;
pub mod rerank;
pub mod ui;

pub use memory_common::{config, error, model, protocol, transcript};

#[cfg(test)]
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../migrations");
