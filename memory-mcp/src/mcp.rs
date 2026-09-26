use rmcp::{ServiceExt, transport::stdio};

use crate::{config, tools};
use memory_common::http_client::HttpMemoryClient;

/// Run the stdio MCP adapter against a remote `memoryd` HTTP endpoint.
///
/// # Errors
///
/// Returns an error if configuration loading or MCP serving fails.
pub async fn run_http(config_path: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    let config = match config_path {
        Some(path) => config::Config::load(std::path::Path::new(path)).map_err(|error| {
            tracing::error!("failed to load config: {error:#?}");
            error
        })?,
        None => config::Config::default(),
    };

    let project = config.guardrails_project.as_deref().filter(|s| !s.trim().is_empty()).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "guardrails_project must select a work project (use 'general' for a general-only session)")
    })?;
    let context = config.resolution_context;
    let client =
        HttpMemoryClient::new(&config.memoryd_url, config.api_token)?.with_context(context.clone());
    let server = tools::MemoryServer::prepare(tools::MemoryBackend::Http(client), project, context)
        .await
        .inspect_err(|error| tracing::error!("guardrail startup failed: {error:#?}"))?;
    start_stdio(server).await
}

async fn start_stdio(server: tools::MemoryServer) -> Result<(), Box<dyn std::error::Error>> {
    tracing::info!("starting MCP stdio server");
    let service = server
        .serve(stdio())
        .await
        .inspect_err(|error| tracing::error!("serving error: {error:#?}"))?;
    service.waiting().await?;

    Ok(())
}
