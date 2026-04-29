//! MCP adapter for the memory server HTTP API.

use rmcp::model::{Implementation, ServerCapabilities, ServerInfo};
use rmcp::{ServerHandler, tool_handler};

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
                "Semantic memory server: store, search, list, update, and delete memories.\n\nUse `memory_rules(tags=...)` with precise `lang:*` and `phase:*` tags to load scoped project rules. Use `memory_search` as the default retrieval entrypoint; it performs graph expansion and may fall back to session-log search. Query expansion and semantic reranking are disabled by default. Use `memory_neighbors` to follow up on promising hits. For cross-project search, use `include_general=true` or `cross_project=true` with `project_allowlist` when appropriate. Use `review_queue` to find `review-needed` items and `review_submit` to record decisions.".into(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::MemoryServer;

    #[test]
    fn test_instructions_contain_key_phrases() {
        let server = MemoryServer::new(crate::tools::MemoryBackend::Http(
            memory_common::http_client::HttpMemoryClient::new("http://localhost:8080", None)
                .unwrap(),
        ));
        let info = server.get_info();
        let instructions = info
            .instructions
            .as_ref()
            .expect("Instructions should be present");

        assert!(instructions.contains("memory_rules"));
        assert!(instructions.contains("memory_search"));
        assert!(instructions.contains("memory_neighbors"));
        assert!(instructions.contains("memory_rules"));
        assert!(instructions.contains("review_queue"));
        assert!(instructions.contains("review_submit"));
    }
}
// Fixed review feedback
