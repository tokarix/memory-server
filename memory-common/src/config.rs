use std::path::Path;

use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct RawConfig {
    #[serde(default)]
    api_token: Option<String>,
    #[serde(default = "default_database_url")]
    database_url: String,
    #[serde(default = "default_dream_model")]
    dream_model: String,
    #[serde(default = "default_embedding_model", alias = "ollama_model")]
    embedding_model: String,
    #[serde(default)]
    embedding_tokenizer_repo: Option<String>,
    #[serde(default)]
    embedding_tokenizer_revision: Option<String>,
    #[serde(default = "default_expand_model")]
    expand_model: String,

    #[serde(alias = "generate_num_ctx")]
    generate_num_ctx: Option<u32>,
    expand_num_ctx: Option<u32>,
    rerank_num_ctx: Option<u32>,
    dream_num_ctx: Option<u32>,

    #[serde(default = "default_ollama_url")]
    ollama_url: String,
    #[serde(default = "default_http_bind")]
    http_bind: String,
    #[serde(default = "default_memoryd_url")]
    memoryd_url: String,
    #[serde(default = "default_rerank_model")]
    rerank_model: String,
}

impl Default for RawConfig {
    fn default() -> Self {
        Self {
            api_token: None,
            database_url: default_database_url(),
            dream_model: default_dream_model(),
            embedding_model: default_embedding_model(),
            embedding_tokenizer_repo: None,
            embedding_tokenizer_revision: None,
            expand_model: default_expand_model(),
            generate_num_ctx: None,
            expand_num_ctx: None,
            rerank_num_ctx: None,
            dream_num_ctx: None,
            ollama_url: default_ollama_url(),
            http_bind: default_http_bind(),
            memoryd_url: default_memoryd_url(),
            rerank_model: default_rerank_model(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(from = "RawConfig")]
pub struct Config {
    pub api_token: Option<String>,
    pub database_url: String,
    pub dream_model: String,
    pub embedding_model: String,
    pub embedding_tokenizer_repo: Option<String>,
    pub embedding_tokenizer_revision: Option<String>,
    pub expand_model: String,
    pub expand_num_ctx: u32,
    pub rerank_num_ctx: u32,
    pub dream_num_ctx: u32,
    pub ollama_url: String,
    pub http_bind: String,
    pub memoryd_url: String,
    pub rerank_model: String,
}

impl From<RawConfig> for Config {
    fn from(raw: RawConfig) -> Self {
        Self {
            api_token: raw.api_token,
            database_url: raw.database_url,
            dream_model: raw.dream_model,
            embedding_model: raw.embedding_model,
            embedding_tokenizer_repo: raw.embedding_tokenizer_repo,
            embedding_tokenizer_revision: raw.embedding_tokenizer_revision,
            expand_model: raw.expand_model,
            expand_num_ctx: raw
                .expand_num_ctx
                .unwrap_or_else(|| raw.generate_num_ctx.unwrap_or(8192)),
            rerank_num_ctx: raw
                .rerank_num_ctx
                .unwrap_or_else(|| raw.generate_num_ctx.unwrap_or(8192)),
            dream_num_ctx: raw
                .dream_num_ctx
                .unwrap_or_else(|| raw.generate_num_ctx.unwrap_or(8192)),
            ollama_url: raw.ollama_url,
            http_bind: raw.http_bind,
            memoryd_url: raw.memoryd_url,
            rerank_model: raw.rerank_model,
        }
    }
}

impl Config {
    /// Load configuration from a TOML file.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read or the TOML cannot be parsed.
    pub fn load(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let contents = std::fs::read_to_string(path)?;
        let config: Self = toml::from_str(&contents)?;
        Ok(config)
    }
}

impl Default for Config {
    fn default() -> Self {
        RawConfig::default().into()
    }
}

fn default_database_url() -> String {
    "postgres://memory:memory@localhost/memory".to_owned()
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
        assert_eq!(config.embedding_tokenizer_repo, None);
        assert_eq!(config.embedding_tokenizer_revision, None);
        assert_eq!(config.expand_model, "llama3.1");
        assert_eq!(config.expand_num_ctx, 8192);
        assert_eq!(config.rerank_num_ctx, 8192);
        assert_eq!(config.dream_num_ctx, 8192);
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
        assert_eq!(config.embedding_tokenizer_repo, None);
        assert_eq!(config.embedding_tokenizer_revision, None);
        assert_eq!(config.expand_model, "llama3.1");
        assert_eq!(config.expand_num_ctx, 8192);
        assert_eq!(config.rerank_num_ctx, 8192);
        assert_eq!(config.dream_num_ctx, 8192);
        assert_eq!(config.ollama_url, "http://localhost:11434");
        assert_eq!(config.rerank_model, "llama3.1");
    }

    #[test]
    fn deserialize_minimal_mcp() {
        let toml = r#"
        memoryd_url = "http://127.0.0.1:8080"
        api_token = "test-token"
        "#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.memoryd_url, "http://127.0.0.1:8080");
        assert_eq!(config.api_token, Some("test-token".to_owned()));
        // Verify other defaults are still applied
        assert_eq!(config.expand_num_ctx, 8192);
    }

    #[test]
    fn config_default_matches_empty_toml() {
        let default = Config::default();
        let empty: Config = toml::from_str("").unwrap();

        assert_eq!(default.api_token, empty.api_token);
        assert_eq!(default.database_url, empty.database_url);
        assert_eq!(default.dream_model, empty.dream_model);
        assert_eq!(default.embedding_model, empty.embedding_model);
        assert_eq!(
            default.embedding_tokenizer_repo,
            empty.embedding_tokenizer_repo
        );
        assert_eq!(
            default.embedding_tokenizer_revision,
            empty.embedding_tokenizer_revision
        );
        assert_eq!(default.expand_model, empty.expand_model);
        assert_eq!(default.expand_num_ctx, empty.expand_num_ctx);
        assert_eq!(default.rerank_num_ctx, empty.rerank_num_ctx);
        assert_eq!(default.dream_num_ctx, empty.dream_num_ctx);
        assert_eq!(default.ollama_url, empty.ollama_url);
        assert_eq!(default.http_bind, empty.http_bind);
        assert_eq!(default.memoryd_url, empty.memoryd_url);
        assert_eq!(default.rerank_model, empty.rerank_model);
    }

    #[test]
    fn deserialize_legacy_fallback() {
        let toml = "generate_num_ctx = 10000";
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.expand_num_ctx, 10000);
        assert_eq!(config.rerank_num_ctx, 10000);
        assert_eq!(config.dream_num_ctx, 10000);
    }

    #[test]
    fn deserialize_mixed_keys() {
        let toml = r"
        generate_num_ctx = 10000
        expand_num_ctx = 3000
        ";
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.expand_num_ctx, 3000);
        assert_eq!(config.rerank_num_ctx, 10000);
        assert_eq!(config.dream_num_ctx, 10000);
    }

    #[test]
    fn deserialize_new_keys_only() {
        let toml = r"
        expand_num_ctx = 1000
        rerank_num_ctx = 2000
        dream_num_ctx = 3000
        ";
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.expand_num_ctx, 1000);
        assert_eq!(config.rerank_num_ctx, 2000);
        assert_eq!(config.dream_num_ctx, 3000);
    }
}
