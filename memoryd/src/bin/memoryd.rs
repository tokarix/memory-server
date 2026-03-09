//! Run the memory server HTTP daemon.

use std::sync::Arc;

use axum::Router;
use tower_http::trace::{DefaultMakeSpan, DefaultOnRequest, DefaultOnResponse, TraceLayer};

use memoryd::{api, app::MemoryApp, config, db, embed};

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
        Some(path) => config::Config::load(std::path::Path::new(&path)).map_err(|error| {
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
            config.embedding_model,
        )),
        config.expand_model,
        config.generate_num_ctx,
        reqwest::Client::new(),
        config.ollama_url,
        config.rerank_model,
    );
    let state = api::ApiState {
        app,
        bearer_token: config.api_token,
    };

    let listener = tokio::net::TcpListener::bind(&config.http_bind).await?;
    let router: Router = api::router(state).layer(
        TraceLayer::new_for_http()
            .make_span_with(DefaultMakeSpan::new().level(tracing::Level::INFO))
            .on_request(DefaultOnRequest::new().level(tracing::Level::INFO))
            .on_response(DefaultOnResponse::new().level(tracing::Level::INFO)),
    );
    tracing::info!("starting memoryd HTTP server on {}", config.http_bind);
    axum::serve(listener, router).await?;

    Ok(())
}
