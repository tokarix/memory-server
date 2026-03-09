use std::path::Path;

use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub api_token: Option<String>,
    #[serde(default = "default_database_url")]
    pub database_url: String,
    #[serde(default = "default_dream_model")]
    pub dream_model: String,
    #[serde(default = "default_embedding_model", alias = "ollama_model")]
    pub embedding_model: String,
    #[serde(default = "default_expand_model")]
    pub expand_model: String,
    #[serde(default = "default_generate_num_ctx")]
    pub generate_num_ctx: u32,
    #[serde(default = "default_ollama_url")]
    pub ollama_url: String,
    #[serde(default = "default_http_bind")]
    pub http_bind: String,
    #[serde(default = "default_memoryd_url")]
    pub memoryd_url: String,
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
            api_token: None,
            database_url: default_database_url(),
            dream_model: default_dream_model(),
            embedding_model: default_embedding_model(),
            expand_model: default_expand_model(),
            generate_num_ctx: default_generate_num_ctx(),
            ollama_url: default_ollama_url(),
            http_bind: default_http_bind(),
            memoryd_url: default_memoryd_url(),
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

fn default_http_bind() -> String {
    "127.0.0.1:8080".to_owned()
}

fn default_memoryd_url() -> String {
    "http://127.0.0.1:8080".to_owned()
}

fn default_expand_model() -> String {
    "llama3.1".to_owned()
}

fn default_embedding_model() -> String {
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
        assert_eq!(config.http_bind, "127.0.0.1:8080");
        assert_eq!(config.memoryd_url, "http://127.0.0.1:8080");
        assert_eq!(config.api_token, None);
        assert_eq!(config.dream_model, "llama3.1");
        assert_eq!(config.embedding_model, "bge-m3");
        assert_eq!(config.expand_model, "llama3.1");
        assert_eq!(config.generate_num_ctx, 8192);
        assert_eq!(config.ollama_url, "http://localhost:11434");
        assert_eq!(config.rerank_model, "llama3.1");
    }

    #[test]
    fn deserialize_partial() {
        let toml = r#"embedding_model = "nomic-embed-text""#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.embedding_model, "nomic-embed-text");
        assert_eq!(config.http_bind, "127.0.0.1:8080");
        assert_eq!(config.memoryd_url, "http://127.0.0.1:8080");
        assert_eq!(
            config.database_url,
            "postgres://memory:memory@localhost/memory"
        );
    }

    #[test]
    fn deserialize_legacy_ollama_model_alias() {
        let toml = r#"ollama_model = "nomic-embed-text""#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.embedding_model, "nomic-embed-text");
    }

    #[test]
    fn deserialize_empty() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(
            config.database_url,
            "postgres://memory:memory@localhost/memory"
        );
        assert_eq!(config.http_bind, "127.0.0.1:8080");
        assert_eq!(config.memoryd_url, "http://127.0.0.1:8080");
        assert_eq!(config.api_token, None);
        assert_eq!(config.dream_model, "llama3.1");
        assert_eq!(config.embedding_model, "bge-m3");
        assert_eq!(config.expand_model, "llama3.1");
        assert_eq!(config.generate_num_ctx, 8192);
        assert_eq!(config.ollama_url, "http://localhost:11434");
        assert_eq!(config.rerank_model, "llama3.1");
    }
}
