use rmcp::{ServiceExt, transport::stdio};

use crate::http_client::HttpMemoryClient;
use crate::{config, tools};

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

    let client = HttpMemoryClient::new(&config.memoryd_url, config.api_token)?;
    let server = tools::MemoryServer::new(tools::MemoryBackend::Http(client));
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
