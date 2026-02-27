use std::sync::Arc;

use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::{ServerHandler, ServiceExt, tool_handler, transport::stdio};

mod config;
mod db;
mod embed;
mod error;
mod model;
mod tools;

#[tool_handler]
impl ServerHandler for tools::MemoryServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "Semantic memory server: store, search, list, update, and delete memories.".into(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let config = match std::env::args().nth(1) {
        Some(path) => config::Config::load(std::path::Path::new(&path)).map_err(|e| {
            tracing::error!("failed to load config: {e:#?}");
            e
        })?,
        None => config::Config::default(),
    };

    tracing::info!(
        "connecting to database: {}",
        config.database_url.split('@').next_back().unwrap_or("?"),
    );
    let pool = db::connect(&config.database_url).await?;

    let embed_client = Arc::new(embed::Client::new(config.ollama_url, config.ollama_model));
    let server = tools::MemoryServer::new(pool, embed_client);

    tracing::info!("starting MCP stdio server");
    let service = server
        .serve(stdio())
        .await
        .inspect_err(|e| tracing::error!("serving error: {e:#?}"))?;
    service.waiting().await?;

    Ok(())
}
