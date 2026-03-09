use std::path::Path;

use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(default = "default_database_url")]
    pub database_url: String,
    #[serde(default = "default_dream_model")]
    pub dream_model: String,
    #[serde(default = "default_expand_model")]
    pub expand_model: String,
    #[serde(default = "default_generate_num_ctx")]
    pub generate_num_ctx: u32,
    #[serde(default = "default_ollama_model")]
    pub ollama_model: String,
    #[serde(default = "default_ollama_url")]
    pub ollama_url: String,
    #[serde(default = "default_rerank_model")]
    pub rerank_model: String,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let contents = std::fs::read_to_string(path)?;
        let config: Self = toml::from_str(&contents)?;
        Ok(config)
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            database_url: default_database_url(),
            dream_model: default_dream_model(),
            expand_model: default_expand_model(),
            generate_num_ctx: default_generate_num_ctx(),
            ollama_model: default_ollama_model(),
            ollama_url: default_ollama_url(),
            rerank_model: default_rerank_model(),
        }
    }
}

fn default_database_url() -> String {
    "postgres://memory:memory@localhost/memory".to_owned()
}

fn default_generate_num_ctx() -> u32 {
    8192
}

fn default_dream_model() -> String {
    "llama3.1".to_owned()
}

fn default_expand_model() -> String {
    "llama3.1".to_owned()
}

fn default_ollama_model() -> String {
    "bge-m3".to_owned()
}

fn default_ollama_url() -> String {
    "http://localhost:11434".to_owned()
}

fn default_rerank_model() -> String {
    "llama3.1".to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config() {
        let config = Config::default();
        assert_eq!(
            config.database_url,
            "postgres://memory:memory@localhost/memory"
        );
        assert_eq!(config.dream_model, "llama3.1");
        assert_eq!(config.expand_model, "llama3.1");
        assert_eq!(config.generate_num_ctx, 8192);
        assert_eq!(config.ollama_model, "bge-m3");
        assert_eq!(config.ollama_url, "http://localhost:11434");
        assert_eq!(config.rerank_model, "llama3.1");
    }

    #[test]
    fn deserialize_partial() {
        let toml = r#"ollama_model = "nomic-embed-text""#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.ollama_model, "nomic-embed-text");
        assert_eq!(
            config.database_url,
            "postgres://memory:memory@localhost/memory"
        );
    }

    #[test]
    fn deserialize_empty() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(
            config.database_url,
            "postgres://memory:memory@localhost/memory"
        );
        assert_eq!(config.dream_model, "llama3.1");
        assert_eq!(config.expand_model, "llama3.1");
        assert_eq!(config.generate_num_ctx, 8192);
        assert_eq!(config.ollama_model, "bge-m3");
        assert_eq!(config.ollama_url, "http://localhost:11434");
        assert_eq!(config.rerank_model, "llama3.1");
    }
}
