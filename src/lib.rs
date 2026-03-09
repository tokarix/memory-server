#![allow(clippy::missing_errors_doc, clippy::must_use_candidate)]

use rmcp::model::{Implementation, ServerCapabilities, ServerInfo};
use rmcp::{ServerHandler, tool_handler};

pub mod api;
pub mod app;
pub mod config;
pub mod db;
pub mod dream;
pub mod embed;
pub mod error;
pub mod expand;
pub mod http_client;
pub mod mcp;
pub mod model;
pub mod ollama;
pub mod rerank;
pub mod tools;
pub mod transcript;

#[tool_handler]
impl ServerHandler for tools::MemoryServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            server_info: Implementation {
                version: format!(
                    "{}-{}",
                    env!("CARGO_PKG_VERSION"),
                    env!("GIT_HASH"),
                ),
                ..Implementation::from_build_env()
            },
            instructions: Some(
                "Semantic memory server: store, search, list, update, and delete memories.\n\nUse `memory_recall` at session start to load core memories (importance >= 0.7) for a project.".into(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}
