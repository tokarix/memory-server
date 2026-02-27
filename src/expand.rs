use crate::ollama;

const PROMPT_TEMPLATE: &str = r#"Given the search query below, generate exactly 2 alternative phrasings that capture the same intent using different words or synonyms. Return ONLY a JSON array of 2 strings, no explanation.

Query: "{query}"

Example output: ["alternative phrasing 1", "alternative phrasing 2"]"#;

pub async fn expand_query(
    http: &reqwest::Client,
    ollama_url: &str,
    model: &str,
    query: &str,
) -> Vec<String> {
    let prompt = PROMPT_TEMPLATE.replace("{query}", query);
    let response = match ollama::generate(http, ollama_url, model, &prompt).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("query expansion failed: {e:#?}");
            return vec![query.to_owned()];
        }
    };
    if let Some(variants) = parse_variants(&response) {
        let mut result = Vec::with_capacity(3);
        result.push(query.to_owned());
        result.extend(variants);
        result
    } else {
        tracing::warn!("query expansion: failed to parse response: {response}");
        vec![query.to_owned()]
    }
}

fn parse_variants(response: &str) -> Option<Vec<String>> {
    // Find the JSON array in the response — the LLM may wrap it in markdown
    let start = response.find('[')?;
    let end = response[start..].find(']')? + start + 1;
    let array_str = &response[start..end];
    let variants: Vec<String> = serde_json::from_str(array_str).ok()?;
    if variants.is_empty() {
        return None;
    }
    // Take at most 2, skip empty strings
    let filtered: Vec<String> = variants
        .into_iter()
        .filter(|s| !s.trim().is_empty())
        .take(2)
        .collect();
    if filtered.is_empty() {
        None
    } else {
        Some(filtered)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_clean_json() {
        let response = r#"["database migration", "schema update"]"#;
        let variants = parse_variants(response).unwrap();
        assert_eq!(variants, vec!["database migration", "schema update"]);
    }

    #[test]
    fn parse_markdown_wrapped() {
        let response =
            "Here are the alternatives:\n```json\n[\"db migration\", \"schema change\"]\n```";
        let variants = parse_variants(response).unwrap();
        assert_eq!(variants, vec!["db migration", "schema change"]);
    }

    #[test]
    fn parse_with_explanation() {
        let response =
            "Sure! Here you go: [\"vector search\", \"semantic similarity\"] Hope that helps!";
        let variants = parse_variants(response).unwrap();
        assert_eq!(variants, vec!["vector search", "semantic similarity"]);
    }

    #[test]
    fn parse_more_than_two() {
        let response = r#"["one", "two", "three", "four"]"#;
        let variants = parse_variants(response).unwrap();
        assert_eq!(variants.len(), 2);
        assert_eq!(variants, vec!["one", "two"]);
    }

    #[test]
    fn parse_single_variant() {
        let response = r#"["only one"]"#;
        let variants = parse_variants(response).unwrap();
        assert_eq!(variants, vec!["only one"]);
    }

    #[test]
    fn parse_empty_array() {
        let response = "[]";
        assert!(parse_variants(response).is_none());
    }

    #[test]
    fn parse_empty_strings() {
        let response = r#"["", "  "]"#;
        assert!(parse_variants(response).is_none());
    }

    #[test]
    fn parse_no_json() {
        let response = "I cannot help with that.";
        assert!(parse_variants(response).is_none());
    }

    #[test]
    fn parse_malformed_json() {
        let response = "[not valid json]";
        assert!(parse_variants(response).is_none());
    }

    #[test]
    fn parse_no_closing_bracket() {
        let response = r#"["unclosed", "array""#;
        assert!(parse_variants(response).is_none());
    }
}
