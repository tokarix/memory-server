//! MCP adapter for the memory server HTTP API.

use std::borrow::Cow;

use rmcp::ServerHandler;
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, ListToolsResult, PaginatedRequestParams,
    ProtocolVersion, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;

pub mod mcp;
pub mod tools;

pub use memory_common::{config, error, model, protocol};

impl ServerHandler for tools::MemoryServer {
    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        // Bound initialization, discovery and inline request negotiation together.
        Cow::Borrowed(ProtocolVersion::known_up_to(&ProtocolVersion::V_2025_11_25))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<rmcp::RoleServer>,
    ) -> Result<CallToolResponse, rmcp::ErrorData> {
        let structured = context
            .protocol_version()
            .is_some_and(|version| version >= ProtocolVersion::V_2025_06_18);
        let context = ToolCallContext::new(self, request, context);
        let name = context.name().to_owned();
        // Preserve 1.5's protocol errors for invalid arguments. ToolRouter::call
        // now converts these into isError results; domain isError results must
        // remain untouched. get() also rejects disabled routes.
        if self.tool_router.get(&name).is_none() {
            return Err(rmcp::ErrorData::invalid_params("tool not found", None));
        }
        let route = self
            .tool_router
            .map
            .get(name.as_str())
            .ok_or_else(|| rmcp::ErrorData::invalid_params("tool not found", None))?;
        let mut response = (route.call)(context).await?;
        if name == "memory_guardrails"
            && structured
            && let CallToolResponse::Complete(result) = &mut response
        {
            result.structured_content = Some(serde_json::to_value(self.guardrail_pack()).map_err(
                |error| {
                    rmcp::ErrorData::from(error::Error::Transport(format!(
                        "serialize guardrails: {error}"
                    )))
                },
            )?);
        }
        Ok(response)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<rmcp::RoleServer>,
    ) -> Result<ListToolsResult, rmcp::ErrorData> {
        self.list_descriptors().await
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.descriptor(name)
    }

    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_protocol_version(ProtocolVersion::V_2025_11_25)
            .with_instructions(format!(
                "{}\n\nContextual guidance: use `memory_rules(tags=...)` with precise `lang:*` and `phase:*` tags. Use `memory_search` for retrieval, `memory_neighbors` for related memories, and `review_queue`/`review_submit` for review work.",
                self.instructions()
            ))
            .with_server_info(
                rmcp::model::Implementation::new(
                    "memory-server",
                    format!(
                        "{}-{}",
                        env!("CARGO_PKG_VERSION"),
                        env!("GIT_HASH"),
                    ),
                ),
            )
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
