use std::collections::HashSet;

use chrono::Utc;
use serde::Deserialize;
use uuid::Uuid;

use crate::model::Category;
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

struct DreamContext<'a> {
    dream_model: &'a str,
    dry_run: bool,
    embed_client: &'a embed::Client,
    http: &'a reqwest::Client,
    num_ctx: u32,
    ollama_url: &'a str,
    pool: &'a sqlx::PgPool,
}

/// Run one dream maintenance cycle across all projects.
///
/// # Errors
///
/// Returns an error if loading, embedding, generation, or persistence fails.
pub async fn run(
    pool: &sqlx::PgPool,
    embed_client: &embed::Client,
    http: &reqwest::Client,
    ollama_url: &str,
    dream_model: &str,
    dry_run: bool,
    num_ctx: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    let context = DreamContext {
        dream_model,
        dry_run,
        embed_client,
        http,
        num_ctx,
        ollama_url,
        pool,
    };
    let projects = db::list_projects(pool).await?;
    tracing::info!(count = projects.len(), "discovered projects");

    // Global graph refresh: load all embeddings and memories across all
    // projects so maintenance edges can cross project boundaries.
    let mut all_embeddings: Vec<(Uuid, Vec<f32>)> = Vec::new();
    let mut all_memories: Vec<crate::model::MemorySummary> = Vec::new();
    for project in &projects {
        all_embeddings.extend(db::all_embeddings(pool, project).await?);
        all_memories.extend(db::list(pool, project, None, 10_000, 0).await?);
    }
    tracing::info!(
        embeddings = all_embeddings.len(),
        memories = all_memories.len(),
        "loaded global embeddings and memories for graph refresh"
    );
    let graph_count = refresh_graph_edges(&context, &all_embeddings, &all_memories).await?;
    tracing::info!(count = graph_count, "global graph edges refreshed");

    // Per-project merge/prune pass.
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

        let merged_ids = apply_merges(&context, &merge_candidates).await?;
        apply_prunes(&context, &stale_candidates, &merged_ids).await?;
    }

    tracing::info!("dream cycle complete");
    Ok(())
}

async fn apply_merges(
    context: &DreamContext<'_>,
    candidates: &[MergeCandidate],
) -> Result<HashSet<Uuid>, Box<dyn std::error::Error>> {
    let mut merged_ids: HashSet<Uuid> = HashSet::new();

    for candidate in candidates {
        if merged_ids.contains(&candidate.id_a) || merged_ids.contains(&candidate.id_b) {
            continue;
        }

        let mem_a = db::get(context.pool, candidate.id_a).await?;
        let mem_b = db::get(context.pool, candidate.id_b).await?;
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

        if context.dry_run {
            continue;
        }

        if matches!(mem_a.category, Category::Plan | Category::Rule)
            || matches!(mem_b.category, Category::Plan | Category::Rule)
        {
            tracing::info!(
                id_a = %candidate.id_a,
                id_b = %candidate.id_b,
                "skipping merge: plan/rule memories are protected"
            );
            continue;
        }

        match llm_merge(
            context.http,
            context.ollama_url,
            context.dream_model,
            context.num_ctx,
            &mem_a,
            &mem_b,
        )
        .await
        {
            Ok(merged) => {
                let embedding = context
                    .embed_client
                    .embed(&merged.summary, &merged.content)
                    .await?;
                db::update(
                    context.pool,
                    candidate.id_a,
                    Some(&merged.content),
                    Some(embedding),
                    Some(&merged.summary),
                    None,
                )
                .await?;
                db::delete(context.pool, candidate.id_b).await?;
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
    context: &DreamContext<'_>,
    candidates: &[StaleCandidate],
    merged_ids: &HashSet<Uuid>,
) -> Result<(), Box<dyn std::error::Error>> {
    for candidate in candidates {
        if merged_ids.contains(&candidate.id) {
            continue;
        }

        let Some(memory) = db::get(context.pool, candidate.id).await? else {
            continue;
        };

        tracing::info!(
            id = %candidate.id,
            staleness = candidate.score,
            summary = memory.summary,
            "stale candidate"
        );

        if context.dry_run {
            continue;
        }

        if matches!(memory.category, Category::Plan | Category::Rule) {
            tracing::info!(
                id = %candidate.id,
                "skipping prune: plan/rule memories are protected"
            );
            continue;
        }

        match llm_prune(
            context.http,
            context.ollama_url,
            context.dream_model,
            context.num_ctx,
            &memory,
        )
        .await
        {
            Ok(decision) => {
                if decision.keep {
                    tracing::info!(
                        id = %candidate.id,
                        reason = decision.reason,
                        "keeping stale memory"
                    );
                } else {
                    db::delete(context.pool, candidate.id).await?;
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

/// Minimum embedding cosine similarity to create a `similar` edge.
const SIMILAR_EDGE_THRESHOLD: f32 = 0.75;
/// Minimum number of shared non-structural tags to create a `related_tag` edge.
const SHARED_TAG_MIN: usize = 2;
/// Tag prefixes that encode structural references (not topical).
const STRUCTURAL_PREFIXES: &[&str] = &[
    "author:",
    "plan:",
    "review",
    "reviewed-by:",
    "reviewed-item:",
    "reviewer:",
    "session:",
    "verdict:",
];

fn is_structural_tag(tag: &str) -> bool {
    STRUCTURAL_PREFIXES.iter().any(|p| tag.starts_with(p))
}

fn topical_tags(memory: &crate::model::MemorySummary) -> Vec<&str> {
    memory
        .tags
        .iter()
        .filter(|t| !is_structural_tag(t))
        .map(String::as_str)
        .collect()
}

async fn refresh_graph_edges(
    context: &DreamContext<'_>,
    embeddings: &[(Uuid, Vec<f32>)],
    memories: &[crate::model::MemorySummary],
) -> Result<usize, Box<dyn std::error::Error>> {
    let now = Utc::now();
    let mut count = 0;

    count += refresh_similar_edges(context, embeddings, memories, now).await?;
    count += refresh_related_tag_edges(context, memories, now).await?;

    // Suppress maintenance edges that were not refreshed this cycle.
    if !context.dry_run {
        let suppressed = db::suppress_stale_maintenance_edges(context.pool, now).await?;
        if suppressed > 0 {
            tracing::info!(count = suppressed, "suppressed stale maintenance edges");
        }
    }

    Ok(count)
}

async fn refresh_similar_edges(
    context: &DreamContext<'_>,
    embeddings: &[(Uuid, Vec<f32>)],
    memories: &[crate::model::MemorySummary],
    now: chrono::DateTime<Utc>,
) -> Result<usize, Box<dyn std::error::Error>> {
    let project_map: std::collections::HashMap<Uuid, &str> = memories
        .iter()
        .map(|m| (m.id, m.project.as_str()))
        .collect();
    let mut count = 0;
    for i in 0..embeddings.len() {
        for j in (i + 1)..embeddings.len() {
            let similarity = embed::cosine_similarity(&embeddings[i].1, &embeddings[j].1);
            if (SIMILAR_EDGE_THRESHOLD..DUPLICATE_THRESHOLD).contains(&similarity) {
                if context.dry_run {
                    tracing::info!(
                        src = %embeddings[i].0,
                        dst = %embeddings[j].0,
                        similarity,
                        "would create similar edge"
                    );
                    continue;
                }
                let src_project = project_map.get(&embeddings[i].0).copied().unwrap_or("");
                let dst_project = project_map.get(&embeddings[j].0).copied().unwrap_or("");
                let edge = crate::model::MemoryEdge {
                    id: Uuid::new_v4(),
                    confidence: f64::from(similarity),
                    created_at: now,
                    dst_id: embeddings[j].0,
                    dst_project: dst_project.to_owned(),
                    evidence: Some(format!("cosine similarity {similarity:.3}")),
                    origin: crate::model::EdgeOrigin::EmbeddingNeighbor,
                    relation: crate::model::EdgeRelation::Similar,
                    src_id: embeddings[i].0,
                    src_project: src_project.to_owned(),
                    suppressed: false,
                    updated_at: now,
                    weight: f64::from(similarity),
                };
                db::upsert_edge(context.pool, &edge).await?;
                let reverse = crate::model::MemoryEdge {
                    id: Uuid::new_v4(),
                    dst_id: embeddings[i].0,
                    dst_project: src_project.to_owned(),
                    src_id: embeddings[j].0,
                    src_project: dst_project.to_owned(),
                    ..edge
                };
                db::upsert_edge(context.pool, &reverse).await?;
                count += 2;
            }
        }
    }
    Ok(count)
}

async fn refresh_related_tag_edges(
    context: &DreamContext<'_>,
    memories: &[crate::model::MemorySummary],
    now: chrono::DateTime<Utc>,
) -> Result<usize, Box<dyn std::error::Error>> {
    let mut count = 0;
    for i in 0..memories.len() {
        let tags_i = topical_tags(&memories[i]);
        if tags_i.len() < SHARED_TAG_MIN {
            continue;
        }
        for j in (i + 1)..memories.len() {
            let tags_j = topical_tags(&memories[j]);
            let shared: Vec<&&str> = tags_i.iter().filter(|t| tags_j.contains(t)).collect();
            if shared.len() >= SHARED_TAG_MIN {
                if context.dry_run {
                    tracing::info!(
                        src = %memories[i].id,
                        dst = %memories[j].id,
                        shared_tags = shared.len(),
                        "would create related_tag edge"
                    );
                    continue;
                }
                #[allow(clippy::cast_precision_loss)]
                let weight = (shared.len() as f64) / (tags_i.len().max(tags_j.len()) as f64);
                let evidence = format!(
                    "shared tags: {}",
                    shared.iter().map(|t| **t).collect::<Vec<_>>().join(", ")
                );
                let edge = crate::model::MemoryEdge {
                    id: Uuid::new_v4(),
                    confidence: weight,
                    created_at: now,
                    dst_id: memories[j].id,
                    dst_project: memories[j].project.clone(),
                    evidence: Some(evidence.clone()),
                    origin: crate::model::EdgeOrigin::SharedTag,
                    relation: crate::model::EdgeRelation::RelatedTag,
                    src_id: memories[i].id,
                    src_project: memories[i].project.clone(),
                    suppressed: false,
                    updated_at: now,
                    weight,
                };
                db::upsert_edge(context.pool, &edge).await?;
                let reverse = crate::model::MemoryEdge {
                    id: Uuid::new_v4(),
                    dst_id: memories[i].id,
                    dst_project: memories[i].project.clone(),
                    src_id: memories[j].id,
                    src_project: memories[j].project.clone(),
                    ..edge
                };
                db::upsert_edge(context.pool, &reverse).await?;
                count += 2;
            }
        }
    }
    Ok(count)
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
        let threshold = STALENESS_THRESHOLD * (1.0 + memory.category.importance());

        if staleness > threshold {
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
    num_ctx: u32,
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

    let response = ollama::generate_json(http, url, model, num_ctx, &prompt).await?;
    let merged: MergeResponse = serde_json::from_str(response.trim())?;
    Ok(merged)
}

async fn llm_prune(
    http: &reqwest::Client,
    url: &str,
    model: &str,
    num_ctx: u32,
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

    let response = ollama::generate_json(http, url, model, num_ctx, &prompt).await?;
    let decision: PruneResponse = serde_json::from_str(response.trim())?;
    Ok(decision)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_structural_tag_identifies_prefixes() {
        assert!(is_structural_tag("author:claude"));
        assert!(is_structural_tag(
            "plan:550e8400-e29b-41d4-a716-446655440000"
        ));
        assert!(is_structural_tag("reviewed-by:codex"));
        assert!(is_structural_tag("reviewed-item:some-id"));
        assert!(is_structural_tag("reviewer:codex"));
        assert!(is_structural_tag("review-needed"));
        assert!(is_structural_tag("session:abc"));
        assert!(is_structural_tag("verdict:approved"));
    }

    #[test]
    fn is_structural_tag_rejects_topical() {
        assert!(!is_structural_tag("sqlx"));
        assert!(!is_structural_tag("rmcp"));
        assert!(!is_structural_tag("database"));
        assert!(!is_structural_tag("architecture"));
        // topic: is the standard topical tag format — not structural
        assert!(!is_structural_tag("topic:memory-graph"));
    }

    #[test]
    fn structural_prefixes_sorted() {
        let mut sorted = STRUCTURAL_PREFIXES.to_vec();
        sorted.sort_unstable();
        assert_eq!(STRUCTURAL_PREFIXES, sorted.as_slice());
    }

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
