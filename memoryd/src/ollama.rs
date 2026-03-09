use serde::{Deserialize, Serialize};

use crate::error::Error;

#[derive(Serialize)]
struct GenerateOptions {
    num_ctx: u32,
}

#[derive(Serialize)]
struct GenerateRequest<'a> {
    model: &'a str,
    options: GenerateOptions,
    prompt: &'a str,
    stream: bool,
}

#[derive(Deserialize)]
struct GenerateResponse {
    response: String,
}

/// Request one text generation from `Ollama`.
///
/// # Errors
///
/// Returns an error if the HTTP request fails or the response cannot be parsed.
pub async fn generate(
    http: &reqwest::Client,
    url: &str,
    model: &str,
    num_ctx: u32,
    prompt: &str,
) -> Result<String, Error> {
    let request = GenerateRequest {
        model,
        options: GenerateOptions { num_ctx },
        prompt,
        stream: false,
    };
    let response = http
        .post(format!("{url}/api/generate"))
        .json(&request)
        .send()
        .await
        .map_err(|e| Error::Embedding(format!("ollama generate: {e:#}")))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "<unreadable>".to_owned());
        return Err(Error::Embedding(format!(
            "ollama generate returned {status}: {body}"
        )));
    }

    let parsed: GenerateResponse = response
        .json()
        .await
        .map_err(|e| Error::Embedding(format!("ollama generate parse: {e:#}")))?;

    Ok(parsed.response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_request_serialization() {
        let request = GenerateRequest {
            model: "llama3.1",
            options: GenerateOptions { num_ctx: 8192 },
            prompt: "hello",
            stream: false,
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["model"], "llama3.1");
        assert_eq!(json["options"]["num_ctx"], 8192);
        assert_eq!(json["prompt"], "hello");
        assert_eq!(json["stream"], false);
    }

    #[test]
    fn generate_response_deserialization() {
        let json = r#"{"response":"merged memory content"}"#;
        let parsed: GenerateResponse = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.response, "merged memory content");
    }
}
