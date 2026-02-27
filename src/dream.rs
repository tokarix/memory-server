use std::collections::HashSet;

use chrono::Utc;
use serde::Deserialize;
use uuid::Uuid;

use crate::{db, embed, ollama};

const DUPLICATE_THRESHOLD: f32 = 0.92;
const RECENT_DAYS: i64 = 30;
const STALENESS_THRESHOLD: f64 = 30.0;

#[derive(Deserialize)]
struct MergeResponse {
    content: String,
    summary: String,
}

#[derive(Deserialize)]
struct PruneResponse {
    keep: bool,
    reason: String,
}

struct MergeCandidate {
    id_a: Uuid,
    id_b: Uuid,
    similarity: f32,
}

struct StaleCandidate {
    id: Uuid,
    score: f64,
}

pub async fn run(
    pool: &sqlx::PgPool,
    embed_client: &embed::Client,
    http: &reqwest::Client,
    ollama_url: &str,
    dream_model: &str,
    dry_run: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let projects = db::list_projects(pool).await?;
    tracing::info!(count = projects.len(), "discovered projects");

    for project in &projects {
        tracing::info!(project, "processing project");
        let embeddings = db::all_embeddings(pool, project).await?;
        tracing::info!(project, count = embeddings.len(), "loaded embeddings");

        let merge_candidates = find_merge_candidates(&embeddings);
        tracing::info!(
            project,
            count = merge_candidates.len(),
            "merge candidates found"
        );

        let stale_candidates = find_stale_candidates(pool, project, &embeddings).await?;
        tracing::info!(
            project,
            count = stale_candidates.len(),
            "stale candidates found"
        );

        let merged_ids = apply_merges(
            pool,
            embed_client,
            http,
            ollama_url,
            dream_model,
            dry_run,
            &merge_candidates,
        )
        .await?;
        apply_prunes(
            pool,
            http,
            ollama_url,
            dream_model,
            dry_run,
            &stale_candidates,
            &merged_ids,
        )
        .await?;
    }

    tracing::info!("dream cycle complete");
    Ok(())
}

async fn apply_merges(
    pool: &sqlx::PgPool,
    embed_client: &embed::Client,
    http: &reqwest::Client,
    ollama_url: &str,
    dream_model: &str,
    dry_run: bool,
    candidates: &[MergeCandidate],
) -> Result<HashSet<Uuid>, Box<dyn std::error::Error>> {
    let mut merged_ids: HashSet<Uuid> = HashSet::new();

    for candidate in candidates {
        if merged_ids.contains(&candidate.id_a) || merged_ids.contains(&candidate.id_b) {
            continue;
        }

        let mem_a = db::get(pool, candidate.id_a).await?;
        let mem_b = db::get(pool, candidate.id_b).await?;
        let (Some(mem_a), Some(mem_b)) = (mem_a, mem_b) else {
            continue;
        };

        tracing::info!(
            id_a = %candidate.id_a,
            id_b = %candidate.id_b,
            similarity = candidate.similarity,
            summary_a = mem_a.summary,
            summary_b = mem_b.summary,
            "merge candidate pair"
        );

        if dry_run {
            continue;
        }

        match llm_merge(http, ollama_url, dream_model, &mem_a, &mem_b).await {
            Ok(merged) => {
                let embedding = embed_client.embed(&merged.summary, &merged.content).await?;
                db::update(
                    pool,
                    candidate.id_a,
                    Some(&merged.content),
                    Some(embedding),
                    Some(&merged.summary),
                    None,
                )
                .await?;
                db::delete(pool, candidate.id_b).await?;
                merged_ids.insert(candidate.id_a);
                merged_ids.insert(candidate.id_b);
                tracing::info!(
                    kept = %candidate.id_a,
                    deleted = %candidate.id_b,
                    summary = merged.summary,
                    "merged memories"
                );
            }
            Err(e) => {
                tracing::warn!(
                    id_a = %candidate.id_a,
                    id_b = %candidate.id_b,
                    error = %e,
                    "merge failed, skipping pair"
                );
            }
        }
    }

    Ok(merged_ids)
}

async fn apply_prunes(
    pool: &sqlx::PgPool,
    http: &reqwest::Client,
    ollama_url: &str,
    dream_model: &str,
    dry_run: bool,
    candidates: &[StaleCandidate],
    merged_ids: &HashSet<Uuid>,
) -> Result<(), Box<dyn std::error::Error>> {
    for candidate in candidates {
        if merged_ids.contains(&candidate.id) {
            continue;
        }

        let Some(memory) = db::get(pool, candidate.id).await? else {
            continue;
        };

        tracing::info!(
            id = %candidate.id,
            staleness = candidate.score,
            summary = memory.summary,
            "stale candidate"
        );

        if dry_run {
            continue;
        }

        match llm_prune(http, ollama_url, dream_model, &memory).await {
            Ok(decision) => {
                if decision.keep {
                    tracing::info!(
                        id = %candidate.id,
                        reason = decision.reason,
                        "keeping stale memory"
                    );
                } else {
                    db::delete(pool, candidate.id).await?;
                    tracing::info!(
                        id = %candidate.id,
                        reason = decision.reason,
                        "pruned stale memory"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    id = %candidate.id,
                    error = %e,
                    "prune review failed, keeping memory"
                );
            }
        }
    }

    Ok(())
}

fn find_merge_candidates(embeddings: &[(Uuid, Vec<f32>)]) -> Vec<MergeCandidate> {
    let mut candidates = Vec::new();
    for i in 0..embeddings.len() {
        for j in (i + 1)..embeddings.len() {
            let similarity = embed::cosine_similarity(&embeddings[i].1, &embeddings[j].1);
            if similarity >= DUPLICATE_THRESHOLD {
                candidates.push(MergeCandidate {
                    id_a: embeddings[i].0,
                    id_b: embeddings[j].0,
                    similarity,
                });
            }
        }
    }
    candidates.sort_by(|a, b| {
        b.similarity
            .partial_cmp(&a.similarity)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    candidates
}

async fn find_stale_candidates(
    pool: &sqlx::PgPool,
    project: &str,
    embeddings: &[(Uuid, Vec<f32>)],
) -> Result<Vec<StaleCandidate>, sqlx::Error> {
    let now = Utc::now();
    let recent_cutoff = now - chrono::Duration::days(RECENT_DAYS);

    let all_memories = db::list(pool, project, None, 10_000, 0).await?;

    let recent_embeddings: Vec<&Vec<f32>> = all_memories
        .iter()
        .filter(|m| m.updated_at >= recent_cutoff)
        .filter_map(|m| {
            embeddings
                .iter()
                .find(|(id, _)| *id == m.id)
                .map(|(_, e)| e)
        })
        .collect();

    let mut candidates = Vec::new();
    for memory in &all_memories {
        if memory.updated_at >= recent_cutoff {
            continue;
        }

        #[allow(clippy::cast_precision_loss)]
        let age_days = (now - memory.updated_at).num_days() as f64;

        let Some(embedding) = embeddings
            .iter()
            .find(|(id, _)| *id == memory.id)
            .map(|(_, e)| e)
        else {
            continue;
        };

        let max_sim_to_recent = recent_embeddings
            .iter()
            .map(|r| f64::from(embed::cosine_similarity(embedding, r)))
            .fold(0.0_f64, f64::max);

        let staleness = age_days * (1.0 - max_sim_to_recent);

        if staleness > STALENESS_THRESHOLD {
            candidates.push(StaleCandidate {
                id: memory.id,
                score: staleness,
            });
        }
    }

    candidates.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(candidates)
}

async fn llm_merge(
    http: &reqwest::Client,
    url: &str,
    model: &str,
    a: &crate::model::MemorySummary,
    b: &crate::model::MemorySummary,
) -> Result<MergeResponse, Box<dyn std::error::Error>> {
    let prompt = format!(
        r#"You are a memory consolidation system. Merge these two similar memories into one.
Keep all unique information. Drop redundant details.

Memory A:
Summary: {}
Content: {}

Memory B:
Summary: {}
Content: {}

Respond with ONLY a JSON object (no markdown, no explanation):
{{"summary": "merged summary", "content": "merged content"}}"#,
        a.summary, a.content, b.summary, b.content
    );

    let response = ollama::generate(http, url, model, &prompt).await?;
    let merged: MergeResponse = serde_json::from_str(response.trim())?;
    Ok(merged)
}

async fn llm_prune(
    http: &reqwest::Client,
    url: &str,
    model: &str,
    memory: &crate::model::MemorySummary,
) -> Result<PruneResponse, Box<dyn std::error::Error>> {
    let prompt = format!(
        r#"You are a memory pruning system. Decide if this old memory is still useful.

Category: {}
Summary: {}
Content: {}
Last updated: {}

Consider: Is this information still likely relevant? Does it contain specific technical details
(error messages, API quirks, config values) that remain useful? Or is it generic/outdated?

Respond with ONLY a JSON object (no markdown, no explanation):
{{"keep": true/false, "reason": "brief explanation"}}"#,
        memory.category,
        memory.summary,
        memory.content,
        memory.updated_at.format("%Y-%m-%d"),
    );

    let response = ollama::generate(http, url, model, &prompt).await?;
    let decision: PruneResponse = serde_json::from_str(response.trim())?;
    Ok(decision)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_merge_candidates_above_threshold() {
        let id_a = Uuid::new_v4();
        let id_b = Uuid::new_v4();
        let id_c = Uuid::new_v4();
        let v1 = vec![1.0, 0.0, 0.0];
        let v2 = vec![0.99, 0.01, 0.0];
        let v3 = vec![0.0, 1.0, 0.0];

        let embeddings = vec![(id_a, v1), (id_b, v2), (id_c, v3)];

        let candidates = find_merge_candidates(&embeddings);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].id_a, id_a);
        assert_eq!(candidates[0].id_b, id_b);
        assert!(candidates[0].similarity >= DUPLICATE_THRESHOLD);
    }

    #[test]
    fn find_merge_candidates_none_similar() {
        let embeddings = vec![
            (Uuid::new_v4(), vec![1.0, 0.0, 0.0]),
            (Uuid::new_v4(), vec![0.0, 1.0, 0.0]),
            (Uuid::new_v4(), vec![0.0, 0.0, 1.0]),
        ];

        let candidates = find_merge_candidates(&embeddings);
        assert!(candidates.is_empty());
    }

    #[test]
    fn find_merge_candidates_sorted_by_similarity() {
        let v1 = vec![1.0, 0.0, 0.0];
        let v2 = vec![0.99, 0.01, 0.0];
        let v3 = vec![0.999, 0.001, 0.0];

        let embeddings = vec![
            (Uuid::new_v4(), v1),
            (Uuid::new_v4(), v2),
            (Uuid::new_v4(), v3),
        ];

        let candidates = find_merge_candidates(&embeddings);
        for i in 1..candidates.len() {
            assert!(candidates[i - 1].similarity >= candidates[i].similarity);
        }
    }

    #[test]
    fn merge_response_deserialization() {
        let json = r#"{"summary": "merged", "content": "merged content"}"#;
        let parsed: MergeResponse = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.summary, "merged");
        assert_eq!(parsed.content, "merged content");
    }

    #[test]
    fn prune_response_deserialization_keep() {
        let json = r#"{"keep": true, "reason": "still relevant"}"#;
        let parsed: PruneResponse = serde_json::from_str(json).unwrap();
        assert!(parsed.keep);
        assert_eq!(parsed.reason, "still relevant");
    }

    #[test]
    fn prune_response_deserialization_discard() {
        let json = r#"{"keep": false, "reason": "outdated info"}"#;
        let parsed: PruneResponse = serde_json::from_str(json).unwrap();
        assert!(!parsed.keep);
        assert_eq!(parsed.reason, "outdated info");
    }
}
