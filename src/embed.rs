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

impl Client {
    pub fn new(url: String, model: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            model,
            url,
        }
    }

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
    fn embed_response_empty() {
        let json = r#"{"embeddings":[]}"#;
        let response: EmbedResponse = serde_json::from_str(json).unwrap();
        assert!(response.embeddings.is_empty());
    }
}
