mod config;

fn main() {
    let config = match std::env::args().nth(1) {
        Some(path) => {
            config::Config::load(std::path::Path::new(&path)).expect("failed to load config")
        }
        None => config::Config::default(),
    };
    tracing::info!(
        "config: db={} ollama={} model={}",
        config.database_url,
        config.ollama_url,
        config.ollama_model,
    );
}
