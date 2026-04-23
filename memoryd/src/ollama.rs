use serde::{Deserialize, Serialize};

use crate::error::Error;

#[derive(Serialize)]
struct GenerateOptions {
    num_ctx: u32,
}

#[derive(Serialize)]
struct GenerateRequest<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    format: Option<serde_json::Value>,
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
    generate_inner(http, url, model, num_ctx, prompt, None).await
}

/// Request one text generation from `Ollama` with structured JSON output.
///
/// Pass a JSON schema to constrain the output shape via grammar-based
/// decoding. The schema is passed directly in the `format` field.
///
/// # Errors
///
/// Returns an error if the HTTP request fails or the response cannot be parsed.
pub async fn generate_schema(
    http: &reqwest::Client,
    url: &str,
    model: &str,
    num_ctx: u32,
    prompt: &str,
    schema: serde_json::Value,
) -> Result<String, Error> {
    generate_inner(http, url, model, num_ctx, prompt, Some(schema)).await
}

async fn generate_inner(
    http: &reqwest::Client,
    url: &str,
    model: &str,
    num_ctx: u32,
    prompt: &str,
    format: Option<serde_json::Value>,
) -> Result<String, Error> {
    let request = GenerateRequest {
        format,
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
            format: None,
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
        assert!(json.get("format").is_none());
    }

    #[test]
    fn generate_request_schema_format() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" }
            },
            "required": ["name"]
        });
        let request = GenerateRequest {
            format: Some(schema),
            model: "llama3.1",
            options: GenerateOptions { num_ctx: 8192 },
            prompt: "hello",
            stream: false,
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["format"]["type"], "object");
        assert_eq!(json["format"]["properties"]["name"]["type"], "string");
    }

    #[test]
    fn generate_request_array_schema_format() {
        let schema = serde_json::json!({
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "score": { "type": "integer", "minimum": 0, "maximum": 10 }
                },
                "required": ["id", "score"],
                "additionalProperties": false
            }
        });
        let request = GenerateRequest {
            format: Some(schema.clone()),
            model: "llama3.1",
            options: GenerateOptions { num_ctx: 8192 },
            prompt: "hello",
            stream: false,
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["format"], schema);
        assert_eq!(json["format"]["type"], "array");
        assert_eq!(json["format"]["items"]["additionalProperties"], false);
    }

    #[test]
    fn generate_response_deserialization() {
        let json = r#"{"response":"merged memory content"}"#;
        let parsed: GenerateResponse = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.response, "merged memory content");
    }
}
