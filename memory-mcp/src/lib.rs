//! MCP adapter for the memory server HTTP API.

use rmcp::model::{Implementation, ServerCapabilities, ServerInfo};
use rmcp::{ServerHandler, tool_handler};

pub mod http_client;
pub mod mcp;
pub mod tools;

pub use memory_common::{config, error, model, protocol};

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
                "Semantic memory server: store, search, list, update, and delete memories.\n\nUse `memory_bootstrap` at session start to load effective rules plus non-rule core memories for a project. Use `memory_rules` when hooks or agents need just the durable rule set.".into(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}
