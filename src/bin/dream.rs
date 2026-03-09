use std::sync::Arc;

use memory_server::{config, db, dream, embed};

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

    let args: Vec<String> = std::env::args().collect();
    let dry_run = args.iter().any(|a| a == "--dry-run");
    let config_path = args
        .iter()
        .find(|a| !a.starts_with('-') && *a != &args[0])
        .map(std::path::PathBuf::from);

    let config = match config_path {
        Some(path) => config::Config::load(&path)?,
        None => config::Config::default(),
    };

    tracing::info!(
        dry_run,
        "connecting to database: {}",
        config.database_url.split('@').next_back().unwrap_or("?"),
    );
    let pool = db::connect(&config.database_url).await?;
    db::migrate(&pool).await?;

    let embed_client = Arc::new(embed::Client::new(
        config.ollama_url.clone(),
        config.ollama_model,
    ));
    let http = reqwest::Client::new();

    dream::run(
        &pool,
        &embed_client,
        &http,
        &config.ollama_url,
        &config.dream_model,
        dry_run,
        config.generate_num_ctx,
    )
    .await?;

    Ok(())
}
