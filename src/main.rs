use std::sync::Arc;

use rmcp::{ServiceExt, transport::stdio};

use memory_server::{config, db, embed, tools};

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

    tracing::info!("running migrations");
    db::migrate(&pool).await?;

    let http = reqwest::Client::new();
    let embed_client = Arc::new(embed::Client::new(
        config.ollama_url.clone(),
        config.ollama_model,
    ));
    let server = tools::MemoryServer::new(
        pool,
        embed_client,
        config.expand_model,
        http,
        config.ollama_url,
    );

    tracing::info!("starting MCP stdio server");
    let service = server
        .serve(stdio())
        .await
        .inspect_err(|e| tracing::error!("serving error: {e:#?}"))?;
    service.waiting().await?;

    Ok(())
}
