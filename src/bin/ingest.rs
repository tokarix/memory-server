use std::io::BufReader;
use std::sync::Arc;

use uuid::Uuid;

use memory_server::{config, db, embed, model, transcript};

const CHUNK_OVERLAP: usize = 200;
const CHUNK_SIZE: usize = 4000;

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
    let positional: Vec<&str> = args[1..]
        .iter()
        .filter(|a| !a.starts_with('-'))
        .map(String::as_str)
        .collect();

    let (config_path, transcript_path) = match positional.len() {
        1 => (None, positional[0]),
        2 => (Some(positional[0]), positional[1]),
        _ => {
            eprintln!("Usage: ingest [--dry-run] [config.toml] <transcript.jsonl>");
            std::process::exit(1);
        }
    };

    let config = match config_path {
        Some(path) => config::Config::load(std::path::Path::new(path))?,
        None => config::Config::default(),
    };

    let file = std::fs::File::open(transcript_path)?;
    let reader = BufReader::new(file);
    let parsed = transcript::parse_jsonl(reader).ok_or("failed to parse transcript")?;

    let text_chunks = transcript::chunk_text(&parsed.content, CHUNK_SIZE, CHUNK_OVERLAP);

    tracing::info!(
        session_id = %parsed.session_id,
        project = %parsed.project,
        cwd = %parsed.cwd,
        summary_len = parsed.summary.len(),
        content_len = parsed.content.len(),
        chunk_count = text_chunks.len(),
        "parsed transcript",
    );

    if dry_run {
        tracing::info!("dry-run mode, not storing");
        println!("session_id: {}", parsed.session_id);
        println!("project: {}", parsed.project);
        println!("cwd: {}", parsed.cwd);
        println!("summary: {}", parsed.summary);
        println!("content_len: {}", parsed.content.len());
        println!("chunk_count: {}", text_chunks.len());
        return Ok(());
    }

    tracing::info!(
        "connecting to database: {}",
        config.database_url.split('@').next_back().unwrap_or("?"),
    );
    let pool = db::connect(&config.database_url).await?;
    db::migrate(&pool).await?;

    let embed_client = Arc::new(embed::Client::new(config.ollama_url, config.ollama_model));

    // Embed summary only for parent session log
    let embedding = embed_client.embed(&parsed.summary, "").await?;

    let log = model::SessionLog {
        id: Uuid::new_v4(),
        content: parsed.content,
        created_at: chrono::Utc::now(),
        cwd: parsed.cwd,
        embedding,
        project: parsed.project,
        session_id: parsed.session_id,
        summary: parsed.summary,
    };

    let stored_id = db::session_log_upsert(&pool, &log).await?;
    tracing::info!(id = %stored_id, session_id = %log.session_id, "session log stored");

    // Embed each chunk and store
    let mut chunks = Vec::with_capacity(text_chunks.len());
    for (i, text) in text_chunks.iter().enumerate() {
        let chunk_embedding = embed_client.embed(text, "").await?;
        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
        chunks.push(model::SessionLogChunk {
            chunk_index: i as i32,
            content: text.clone(),
            embedding: chunk_embedding,
            id: Uuid::new_v4(),
            session_log_id: stored_id,
        });
    }

    db::session_log_chunks_replace(&pool, stored_id, &chunks).await?;
    tracing::info!(
        id = %stored_id,
        chunk_count = chunks.len(),
        "session log chunks stored"
    );

    Ok(())
}
