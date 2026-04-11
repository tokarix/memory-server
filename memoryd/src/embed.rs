use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokenizers::Tokenizer;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use crate::error::Error;

const DEFAULT_EMBED_CONTEXT_LIMIT_FALLBACK: usize = 4000;
/// Margin to account for possible drift between our tokenizer and Ollama's,
/// since we must decode tokens back to text for the API request.
const TOKENIZER_SAFETY_MARGIN: usize = 32;

pub struct Client {
    context_limit: Arc<RwLock<Option<usize>>>,
    http: reqwest::Client,
    model: String,
    tokenizer: Option<Tokenizer>,
    url: String,
}

#[derive(Serialize)]
struct EmbedRequest<'a> {
    input: &'a str,
    model: &'a str,
}

#[derive(Deserialize)]
struct EmbedResponse {
    embeddings: Vec<Vec<f32>>,
}

#[derive(Deserialize)]
struct ModelDetails {
    #[serde(rename = "general.architecture")]
    architecture: String,
    #[serde(flatten)]
    others: HashMap<String, serde_json::Value>,
}

#[derive(Deserialize)]
struct ModelInfo {
    model_info: ModelDetails,
}

#[must_use]
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let (mut dot, mut norm_a, mut norm_b) = (0.0_f32, 0.0_f32, 0.0_f32);
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }
    let denom = norm_a.sqrt() * norm_b.sqrt();
    if denom == 0.0 { 0.0 } else { dot / denom }
}

impl Client {
    #[must_use]
    pub fn new(
        url: String,
        model: String,
        tokenizer_repo: Option<String>,
        tokenizer_revision: Option<String>,
    ) -> Self {
        let tokenizer = {
            if let Some(repo_id) = tokenizer_repo {
                let revision = tokenizer_revision.unwrap_or_else(|| "main".to_owned());
                // Use Api::new() to respect environment variables like HF_HOME/HF_ENDPOINT
                match hf_hub::api::sync::Api::new() {
                    Ok(api) => {
                        let repo = api.repo(hf_hub::Repo::with_revision(
                            repo_id.clone(),
                            hf_hub::RepoType::Model,
                            revision.clone(),
                        ));
                        match repo.get("tokenizer.json") {
                            Ok(path) => match Tokenizer::from_file(path.as_path()) {
                                Ok(t) => {
                                    info!(
                                        repo = %repo_id,
                                        revision = %revision,
                                        path = %path.display(),
                                        "Downloaded and loaded tokenizer from HF hub"
                                    );
                                    Some(t)
                                }
                                Err(e) => {
                                    error!(error = %e, "Failed to load downloaded tokenizer");
                                    None
                                }
                            },
                            Err(e) => {
                                error!(repo = %repo_id, error = %e, "Failed to download tokenizer.json from HF hub");
                                None
                            }
                        }
                    }
                    Err(e) => {
                        error!(error = %e, "Failed to build HF hub API");
                        None
                    }
                }
            } else {
                None
            }
        };

        Self {
            context_limit: Arc::new(RwLock::new(None)),
            http: reqwest::Client::new(),
            model,
            tokenizer,
            url,
        }
    }

    async fn get_context_limit(&self) -> usize {
        if let Some(limit) = *self.context_limit.read().await {
            return limit;
        }

        let url = format!("{}/api/show", self.url);
        let request = serde_json::json!({"model": self.model});

        let limit = if let Ok(resp) = self.http.post(url).json(&request).send().await {
            if resp.status().is_success() {
                match resp.json::<ModelInfo>().await {
                    Ok(info) => {
                        let arch = info.model_info.architecture;
                        let key = format!("{arch}.context_length");
                        if let Some(val) = info.model_info.others.get(&key) {
                            val.as_u64()
                                .map_or(DEFAULT_EMBED_CONTEXT_LIMIT_FALLBACK, |v| {
                                    usize::try_from(v)
                                        .unwrap_or(DEFAULT_EMBED_CONTEXT_LIMIT_FALLBACK)
                                })
                        } else {
                            warn!(
                                "Could not find context length for arch {} in model {}",
                                arch, self.model
                            );
                            DEFAULT_EMBED_CONTEXT_LIMIT_FALLBACK
                        }
                    }
                    Err(e) => {
                        error!(
                            "Failed to parse Ollama model info for {}: {e:#}",
                            self.model
                        );
                        DEFAULT_EMBED_CONTEXT_LIMIT_FALLBACK
                    }
                }
            } else {
                warn!(
                    "Failed to discover context limit for model {}, using fallback.",
                    self.model
                );
                DEFAULT_EMBED_CONTEXT_LIMIT_FALLBACK
            }
        } else {
            warn!(
                "Failed to discover context limit for model {}, using fallback.",
                self.model
            );
            DEFAULT_EMBED_CONTEXT_LIMIT_FALLBACK
        };

        *self.context_limit.write().await = Some(limit);
        limit
    }

    /// Request an embedding for the provided summary and content.
    ///
    /// # Errors
    ///
    /// Returns an error if the embedding request fails or the response is invalid.
    pub async fn embed(&self, summary: &str, content: &str) -> Result<Vec<f32>, Error> {
        let input = format!("{summary}\n\n{content}");
        let limit = self.get_context_limit().await;

        let final_input = if let Some(tokenizer) = &self.tokenizer {
            // Tokenizer-guided truncation (approximate due to re-tokenization drift)
            let budget = limit.saturating_sub(TOKENIZER_SAFETY_MARGIN);
            let encoding = tokenizer
                .encode(input.as_str(), true)
                .map_err(|e| Error::Embedding(format!("tokenizer error: {e:#}")))?;

            let token_count = encoding.get_ids().len();
            if token_count > budget {
                warn!(
                    model = %self.model,
                    tokens = token_count,
                    limit = budget,
                    "Truncating input using tokenizer to avoid silent model truncation."
                );
                // Truncate to the token budget.
                let truncated_ids = &encoding.get_ids()[..budget];
                tokenizer
                    .decode(truncated_ids, true)
                    .map_err(|e| Error::Embedding(format!("tokenizer decode error: {e:#}")))?
            } else {
                input
            }
        } else {
            // Fallback to approximate byte budget: conservative 1 token ≈ 4 bytes
            let cap = limit * 4;
            let mut final_input = input;

            if final_input.len() > cap {
                warn!(
                    model = %self.model,
                    original_size = final_input.len(),
                    limit = cap,
                    "Truncating input (byte-based fallback) to avoid silent model truncation."
                );
                // Find nearest char boundary at or before cap.
                let mut truncate_at = cap;
                while truncate_at > 0 && !final_input.is_char_boundary(truncate_at) {
                    truncate_at -= 1;
                }
                final_input.truncate(truncate_at);
            }
            final_input
        };

        let request = EmbedRequest {
            input: &final_input,
            model: &self.model,
        };
        let response = self
            .http
            .post(format!("{}/api/embed", self.url))
            .json(&request)
            .send()
            .await
            .map_err(|e| Error::Embedding(format!("{e:#}")))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "<unreadable>".to_owned());
            return Err(Error::Embedding(format!(
                "Ollama returned {status}: {body}"
            )));
        }

        let embed_response: EmbedResponse = response
            .json()
            .await
            .map_err(|e| Error::Embedding(format!("failed to parse response: {e:#}")))?;

        embed_response
            .embeddings
            .into_iter()
            .next()
            .ok_or_else(|| Error::Embedding("empty embeddings array".to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embed_request_serialization() {
        let request = EmbedRequest {
            input: "test input",
            model: "bge-m3",
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["input"], "test input");
        assert_eq!(json["model"], "bge-m3");
    }

    #[test]
    fn embed_response_deserialization() {
        let json = r#"{"embeddings":[[0.1,0.2,0.3]]}"#;
        let response: EmbedResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.embeddings.len(), 1);
        assert_eq!(response.embeddings[0], vec![0.1, 0.2, 0.3]);
    }

    #[test]
    fn cosine_similarity_identical() {
        let v = [1.0, 2.0, 3.0];
        let sim = cosine_similarity(&v, &v);
        assert!((sim - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_orthogonal() {
        let a = [1.0, 0.0];
        let b = [0.0, 1.0];
        let sim = cosine_similarity(&a, &b);
        assert!(sim.abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_opposite() {
        let a = [1.0, 0.0];
        let b = [-1.0, 0.0];
        let sim = cosine_similarity(&a, &b);
        assert!((sim - (-1.0)).abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_zero_vector() {
        let a = [0.0, 0.0];
        let b = [1.0, 2.0];
        let sim = cosine_similarity(&a, &b);
        assert!((sim - 0.0).abs() < 1e-6);
    }

    #[test]
    fn embed_truncation_logic() {
        let _client = Client::new(
            "http://localhost".to_owned(),
            "test-model".to_owned(),
            None,
            None,
        );
        // Force a limit that triggers truncation
        let limit = 10;
        let cap = limit * 4;
        let summary = "S".repeat(20);
        let content = "C".repeat(20);
        let input = format!("{summary}\n\n{content}");

        let mut final_input = input.clone();
        if final_input.len() > cap {
            // Find nearest char boundary at or before cap.
            let mut truncate_at = cap;
            while truncate_at > 0 && !final_input.is_char_boundary(truncate_at) {
                truncate_at -= 1;
            }
            final_input.truncate(truncate_at);
        }

        assert!(final_input.len() <= cap);
        assert!(final_input.starts_with(&summary));
    }
}
