//! Dry-run-first conversion of the pinned historical Rust storage Rules.

use std::path::Path;

use memoryd::{config, db, embed, policy_conversion};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut apply = false;
    let mut config_path = None;
    let mut manifest_path = None;
    let mut args = std::env::args().skip(1);
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--apply" => apply = true,
            "--config" => {
                config_path = Some(args.next().ok_or("--config requires a path")?);
            }
            value if value.starts_with('-') => {
                return Err(format!("unknown argument {value}").into());
            }
            value => {
                if manifest_path.replace(value.to_owned()).is_some() {
                    return Err("provide exactly one manifest path".into());
                }
            }
        }
    }
    let manifest_path = manifest_path
        .ok_or("usage: convert_storage_policies [--config config.toml] [--apply] manifest.json")?;
    let manifest: policy_conversion::ConversionManifest =
        serde_json::from_str(&std::fs::read_to_string(&manifest_path)?)?;
    manifest.validate()?;
    let config = match config_path {
        Some(path) => config::Config::load(Path::new(&path))?,
        None => config::Config::default(),
    };
    let pool = db::connect(&config.database_url).await?;
    let preview = policy_conversion::dry_run(&pool, &manifest).await?;
    println!("{}", serde_json::to_string_pretty(&preview)?);
    if !apply {
        return Ok(());
    }
    let embeddings = if preview.already_applied {
        [Vec::new(), Vec::new(), Vec::new()]
    } else {
        let client = embed::Client::new(
            config.ollama_url,
            config.embedding_model,
            config.embedding_tokenizer_repo,
            config.embedding_tokenizer_revision,
        );
        let host = client
            .embed(&manifest.host_active.summary, &manifest.host_active.content)
            .await?;
        let guidance = client
            .embed(
                &manifest.guidance_active.summary,
                &manifest.guidance_active.content,
            )
            .await?;
        let container = client
            .embed(
                &manifest.container_active.summary,
                &manifest.container_active.content,
            )
            .await?;
        [host, guidance, container]
    };
    let result = policy_conversion::apply(&pool, &manifest, embeddings).await?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}
