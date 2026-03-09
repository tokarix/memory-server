use std::fmt::Write;

use uuid::Uuid;

use crate::{model, ollama};

const PROMPT_TEMPLATE: &str = r#"You are a relevance scoring system. Rate how relevant each memory is to the given search query.

Query: "{query}"

Memories to score:
{candidates}

For each memory, assign a relevance score from 0 (completely irrelevant) to 10 (perfectly relevant).

Return ONLY a JSON array of objects with "id" and "score" fields, no explanation.
Example: [{"id": "uuid-1", "score": 8}, {"id": "uuid-2", "score": 3}]"#;

pub async fn rerank(
    http: &reqwest::Client,
    ollama_url: &str,
    model: &str,
    num_ctx: u32,
    query: &str,
    results: Vec<(model::MemorySummary, f64)>,
) -> Vec<(model::MemorySummary, f64)> {
    if results.is_empty() {
        return results;
    }

    let mut candidates = String::new();
    for (m, _) in &results {
        let _ = write!(
            candidates,
            "- ID: {}\n  Summary: {}\n  Content: {}\n\n",
            m.id, m.summary, m.content
        );
    }

    let prompt = PROMPT_TEMPLATE
        .replace("{query}", query)
        .replace("{candidates}", &candidates);

    let response = match ollama::generate(http, ollama_url, model, num_ctx, &prompt).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("rerank LLM call failed: {e:#?}");
            return results;
        }
    };

    let Some(scores) = parse_scores(&response) else {
        tracing::warn!("rerank: failed to parse LLM response: {response}");
        return results;
    };

    blend(results, &scores)
}

fn parse_scores(response: &str) -> Option<Vec<(Uuid, u8)>> {
    let start = response.find('[')?;
    let end = response[start..].find(']')? + start + 1;
    let array_str = &response[start..end];

    let raw: Vec<serde_json::Value> = serde_json::from_str(array_str).ok()?;
    if raw.is_empty() {
        return None;
    }

    let mut scores = Vec::with_capacity(raw.len());
    for item in &raw {
        let id_str = item.get("id")?.as_str()?;
        let id: Uuid = id_str.parse().ok()?;
        let score_val = item.get("score")?.as_u64()?;
        #[allow(clippy::cast_possible_truncation)]
        let score = (score_val.min(10)) as u8;
        scores.push((id, score));
    }

    if scores.is_empty() {
        None
    } else {
        Some(scores)
    }
}

fn blend(
    results: Vec<(model::MemorySummary, f64)>,
    scores: &[(Uuid, u8)],
) -> Vec<(model::MemorySummary, f64)> {
    use std::collections::HashMap;

    let score_map: HashMap<Uuid, u8> = scores.iter().copied().collect();

    // Normalize RRF scores: divide by max
    let max_rrf = results
        .iter()
        .map(|(_, s)| *s)
        .fold(f64::NEG_INFINITY, f64::max);

    if max_rrf <= 0.0 {
        return results;
    }

    let mut blended: Vec<(model::MemorySummary, f64)> = results
        .into_iter()
        .enumerate()
        .map(|(rank, (memory, rrf_score))| {
            let norm_rrf = rrf_score / max_rrf;
            let llm_raw = score_map.get(&memory.id).copied().unwrap_or(5);
            let norm_llm = f64::from(llm_raw) / 10.0;

            let (w_rrf, w_llm) = match rank + 1 {
                1..=3 => (0.75, 0.25),
                4..=10 => (0.60, 0.40),
                _ => (0.40, 0.60),
            };

            let importance = memory.category.importance();
            let blended_score = (w_rrf * norm_rrf + w_llm * norm_llm) * (0.5 + importance);
            (memory, blended_score)
        })
        .collect();

    blended.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    blended
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use crate::model::Category;

    use super::*;

    fn make_summary(id: Uuid) -> model::MemorySummary {
        model::MemorySummary {
            id,
            category: Category::Context,
            content: "test content".to_owned(),
            created_at: Utc.with_ymd_and_hms(2025, 6, 15, 12, 0, 0).unwrap(),
            project: "test".to_owned(),
            summary: "test summary".to_owned(),
            tags: vec![],
            updated_at: Utc.with_ymd_and_hms(2025, 6, 15, 12, 0, 0).unwrap(),
        }
    }

    #[test]
    fn parse_scores_clean_json() {
        let response = r#"[{"id": "00000000-0000-0000-0000-000000000001", "score": 8}, {"id": "00000000-0000-0000-0000-000000000002", "score": 3}]"#;
        let scores = parse_scores(response).unwrap();
        assert_eq!(scores.len(), 2);
        assert_eq!(scores[0], (Uuid::from_u128(1), 8));
        assert_eq!(scores[1], (Uuid::from_u128(2), 3));
    }

    #[test]
    fn parse_scores_markdown_wrapped() {
        let response = "Here are the scores:\n```json\n[{\"id\": \"00000000-0000-0000-0000-000000000001\", \"score\": 7}]\n```";
        let scores = parse_scores(response).unwrap();
        assert_eq!(scores.len(), 1);
        assert_eq!(scores[0], (Uuid::from_u128(1), 7));
    }

    #[test]
    fn parse_scores_malformed() {
        assert!(parse_scores("not json at all").is_none());
        assert!(parse_scores("[invalid]").is_none());
        assert!(parse_scores("[]").is_none());
    }

    #[test]
    fn parse_scores_empty() {
        assert!(parse_scores("").is_none());
    }

    #[test]
    fn parse_scores_clamped_to_10() {
        let response = r#"[{"id": "00000000-0000-0000-0000-000000000001", "score": 15}]"#;
        let scores = parse_scores(response).unwrap();
        assert_eq!(scores[0].1, 10);
    }

    #[test]
    fn blend_tier1_weights() {
        // Ranks 1-3: 75% RRF / 25% LLM
        let id1 = Uuid::from_u128(1);
        let id2 = Uuid::from_u128(2);
        let results = vec![
            (make_summary(id1), 1.0), // rank 1, max RRF
            (make_summary(id2), 0.5), // rank 2
        ];
        let scores = vec![(id1, 5_u8), (id2, 10_u8)];

        let blended = blend(results, &scores);

        // id1: norm_rrf=1.0, norm_llm=0.5 -> 0.75*1.0 + 0.25*0.5 = 0.875
        // id2: norm_rrf=0.5, norm_llm=1.0 -> 0.75*0.5 + 0.25*1.0 = 0.625
        assert_eq!(blended[0].0.id, id1);
        assert!((blended[0].1 - 0.875).abs() < 1e-10);
        assert_eq!(blended[1].0.id, id2);
        assert!((blended[1].1 - 0.625).abs() < 1e-10);
    }

    #[test]
    fn blend_tier2_weights() {
        // Ranks 4-10: 60% RRF / 40% LLM
        // Create 5 items so items at index 3 and 4 are in the 4-10 tier
        let ids: Vec<Uuid> = (1..=5).map(Uuid::from_u128).collect();
        let results: Vec<_> = ids
            .iter()
            .enumerate()
            .map(|(i, &id)| {
                #[allow(clippy::cast_precision_loss)]
                let score = 1.0 - (i as f64 * 0.1);
                (make_summary(id), score)
            })
            .collect();
        // Give item at rank 5 (index 4) a high LLM score
        let scores = vec![(ids[4], 10_u8), (ids[3], 10_u8)];

        let blended = blend(results, &scores);

        // Item at original rank 4 (index 3): norm_rrf=0.7/1.0=0.7, norm_llm=1.0
        // blended = 0.60*0.7 + 0.40*1.0 = 0.82
        let rank4_item = blended.iter().find(|(m, _)| m.id == ids[3]).unwrap();
        assert!((rank4_item.1 - 0.82).abs() < 1e-10);
    }

    #[test]
    fn blend_tier3_weights() {
        // Ranks 11+: 40% RRF / 60% LLM
        let ids: Vec<Uuid> = (1..=12).map(Uuid::from_u128).collect();
        let results: Vec<_> = ids
            .iter()
            .enumerate()
            .map(|(i, &id)| {
                #[allow(clippy::cast_precision_loss)]
                let score = 1.0 - (i as f64 * 0.05);
                (make_summary(id), score)
            })
            .collect();
        // Item at rank 12 (index 11): RRF = 1.0 - 0.55 = 0.45
        let scores = vec![(ids[11], 10_u8)];

        let blended = blend(results, &scores);

        // norm_rrf = 0.45/1.0 = 0.45, norm_llm = 1.0
        // blended = 0.40*0.45 + 0.60*1.0 = 0.78
        let rank12_item = blended.iter().find(|(m, _)| m.id == ids[11]).unwrap();
        assert!((rank12_item.1 - 0.78).abs() < 1e-10);
    }

    fn make_summary_with_category(id: Uuid, category: Category) -> model::MemorySummary {
        model::MemorySummary {
            id,
            category,
            content: "test content".to_owned(),
            created_at: Utc.with_ymd_and_hms(2025, 6, 15, 12, 0, 0).unwrap(),
            project: "test".to_owned(),
            summary: "test summary".to_owned(),
            tags: vec![],
            updated_at: Utc.with_ymd_and_hms(2025, 6, 15, 12, 0, 0).unwrap(),
        }
    }

    #[test]
    fn blend_importance_boost() {
        // Two memories with identical RRF and LLM scores but different categories
        let id_rule = Uuid::from_u128(1);
        let id_context = Uuid::from_u128(2);
        let results = vec![
            (make_summary_with_category(id_rule, Category::Rule), 1.0),
            (
                make_summary_with_category(id_context, Category::Context),
                1.0,
            ),
        ];
        let scores = vec![(id_rule, 5_u8), (id_context, 5_u8)];

        let blended = blend(results, &scores);

        // Both have same base: 0.75*1.0 + 0.25*0.5 = 0.875
        // Rule: 0.875 * (0.5 + 0.9) = 0.875 * 1.4 = 1.225
        // Context: 0.875 * (0.5 + 0.5) = 0.875 * 1.0 = 0.875
        assert_eq!(blended[0].0.id, id_rule);
        assert!((blended[0].1 - 1.225).abs() < 1e-10);
        assert_eq!(blended[1].0.id, id_context);
        assert!((blended[1].1 - 0.875).abs() < 1e-10);
    }

    #[test]
    fn blend_missing_scores_default_to_5() {
        let id1 = Uuid::from_u128(1);
        let results = vec![(make_summary(id1), 1.0)];
        // No scores provided for id1
        let scores: Vec<(Uuid, u8)> = vec![];

        let blended = blend(results, &scores);

        // norm_rrf=1.0, norm_llm=0.5 (default 5/10), rank 1: 0.75*1.0 + 0.25*0.5 = 0.875
        assert!((blended[0].1 - 0.875).abs() < 1e-10);
    }
}
