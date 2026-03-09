use std::sync::Arc;

use rmcp::{ServiceExt, transport::stdio};

use crate::http_client::HttpMemoryClient;
use crate::{app::MemoryApp, config, db, embed, tools};

pub async fn run(config_path: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    let config = match config_path {
        Some(path) => config::Config::load(std::path::Path::new(path)).map_err(|error| {
            tracing::error!("failed to load config: {error:#?}");
            error
        })?,
        None => config::Config::default(),
    };

    tracing::info!(
        "connecting to database: {}",
        config.database_url.split('@').next_back().unwrap_or("?"),
    );
    let pool = db::connect(&config.database_url).await?;

    tracing::info!("running migrations");
    db::migrate(&pool).await?;

    let app = MemoryApp::new(
        pool,
        Arc::new(embed::Client::new(
            config.ollama_url.clone(),
            config.ollama_model,
        )),
        config.expand_model,
        config.generate_num_ctx,
        reqwest::Client::new(),
        config.ollama_url,
        config.rerank_model,
    );
    let server = tools::MemoryServer::new(tools::MemoryBackend::Local(app));
    start_stdio(server).await
}

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
