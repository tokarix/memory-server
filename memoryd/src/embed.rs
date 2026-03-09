use serde::{Deserialize, Serialize};

use crate::error::Error;

pub struct Client {
    http: reqwest::Client,
    model: String,
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
    pub fn new(url: String, model: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            model,
            url,
        }
    }

    /// Request an embedding for the provided summary and content.
    ///
    /// # Errors
    ///
    /// Returns an error if the embedding request fails or the response is invalid.
    pub async fn embed(&self, summary: &str, content: &str) -> Result<Vec<f32>, Error> {
        let input = format!("{summary}\n\n{content}");
        let request = EmbedRequest {
            input: &input,
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
    fn embed_response_empty() {
        let json = r#"{"embeddings":[]}"#;
        let response: EmbedResponse = serde_json::from_str(json).unwrap();
        assert!(response.embeddings.is_empty());
    }
}
