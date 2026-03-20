use std::collections::HashSet;

use chrono::Utc;
use sqlx::PgPool;
use uuid::Uuid;

use crate::model::{EdgeOrigin, EdgeRelation, MemoryEdge, MemorySummary};

/// Default weight threshold for graph expansion — only follow strong edges.
const EXPANSION_WEIGHT_THRESHOLD: f64 = 0.5;
/// Maximum neighbors to fetch per seed memory during expansion.
const EXPANSION_NEIGHBOR_LIMIT: i64 = 10;
/// Decay factor applied to expanded memory scores per hop.
const HOP_DECAY: f64 = 0.7;
/// Additional discount for memories from the `general` project.
const GENERAL_DISCOUNT: f64 = 0.9;
/// Additional discount for memories from foreign projects.
const FOREIGN_DISCOUNT: f64 = 0.5;

/// Graph expansion scope policy.
pub struct ExpansionPolicy {
    pub cross_project: bool,
    pub graph_hops: u32,
    pub include_general: bool,
    pub project_allowlist: Option<Vec<String>>,
    pub source_project: String,
}

/// Expand a seed set of memories by following graph edges.
///
/// Returns expanded memories (not already in seeds) with decayed scores.
/// Inserted between outer RRF and reranking in the search pipeline.
///
/// # Errors
///
/// Returns an error if database queries fail.
pub async fn graph_expand(
    pool: &PgPool,
    seeds: &[(MemorySummary, f64)],
    policy: &ExpansionPolicy,
) -> Result<Vec<(MemorySummary, f64)>, sqlx::Error> {
    if policy.graph_hops == 0 || seeds.is_empty() {
        return Ok(Vec::new());
    }

    let seed_ids: HashSet<Uuid> = seeds.iter().map(|(m, _)| m.id).collect();
    let mut expanded: Vec<(MemorySummary, f64)> = Vec::new();
    let mut seen: HashSet<Uuid> = seed_ids.clone();

    // Current frontier: memories to expand from, with their scores.
    let mut frontier: Vec<(Uuid, f64)> = seeds.iter().map(|(m, s)| (m.id, *s)).collect();

    for _hop in 0..policy.graph_hops {
        let mut next_frontier: Vec<(Uuid, f64)> = Vec::new();
        for (node_id, node_score) in &frontier {
            let neighbors =
                crate::db::list_neighbors(pool, *node_id, EXPANSION_NEIGHBOR_LIMIT).await?;
            for (edge, neighbor) in neighbors {
                if seen.contains(&neighbor.id) || edge.weight < EXPANSION_WEIGHT_THRESHOLD {
                    continue;
                }
                if !is_in_scope(&neighbor.project, policy) {
                    continue;
                }
                let discount = project_discount(&neighbor.project, policy);
                let score = node_score * HOP_DECAY * discount;
                seen.insert(neighbor.id);
                next_frontier.push((neighbor.id, score));
                expanded.push((neighbor, score));
            }
        }
        if next_frontier.is_empty() {
            break;
        }
        frontier = next_frontier;
    }

    // Sort by score descending.
    expanded.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    Ok(expanded)
}

fn is_in_scope(project: &str, policy: &ExpansionPolicy) -> bool {
    // Same project is always allowed.
    if project == policy.source_project {
        return true;
    }
    // General project allowed if opted in.
    if project == crate::app::GENERAL_RULE_PROJECT {
        return policy.include_general;
    }
    // Foreign project requires cross_project opt-in.
    if !policy.cross_project {
        return false;
    }
    // If allowlist is set, check it.
    if let Some(ref allowlist) = policy.project_allowlist {
        return allowlist.iter().any(|p| p == project);
    }
    true
}

fn project_discount(project: &str, policy: &ExpansionPolicy) -> f64 {
    if project == policy.source_project {
        1.0
    } else if project == crate::app::GENERAL_RULE_PROJECT {
        GENERAL_DISCOUNT
    } else {
        FOREIGN_DISCOUNT
    }
}

/// Structural tag prefixes that encode explicit UUID references.
const STRUCTURAL_TAG_PREFIXES: &[&str] = &["plan:", "reviewed-item:"];

/// Build deterministic edges for a memory at write time.
///
/// Scans the memory's content for UUID references and its tags for
/// structural references like `plan:<uuid>` and `reviewed-item:<uuid>`.
///
/// # Errors
///
/// Returns an error if upserting edges fails.
pub async fn build_write_time_edges(
    pool: &PgPool,
    memory: &MemorySummary,
) -> Result<usize, sqlx::Error> {
    // Clear existing write-time edges before rebuilding so stale
    // references are removed when content or tags change.
    crate::db::delete_edges_by_origins(
        pool,
        memory.id,
        &[EdgeOrigin::ContentUuidRef, EdgeOrigin::StructuralTagRef],
    )
    .await?;

    let mut count = 0;

    // Build edges from UUID mentions in content.
    for target_id in extract_uuids_from_text(&memory.content) {
        if target_id == memory.id {
            continue;
        }
        if let Some(target) = crate::db::get(pool, target_id).await? {
            upsert_reference_edge(
                pool,
                memory,
                &target,
                EdgeOrigin::ContentUuidRef,
                Some(format!("UUID {target_id} found in content")),
            )
            .await?;
            count += 1;
        }
    }

    // Build edges from structural tags.
    for tag in &memory.tags {
        for prefix in STRUCTURAL_TAG_PREFIXES {
            if let Some(uuid_str) = tag.strip_prefix(prefix)
                && let Ok(target_id) = uuid_str.parse::<Uuid>()
            {
                if target_id == memory.id {
                    continue;
                }
                if let Some(target) = crate::db::get(pool, target_id).await? {
                    upsert_reference_edge(
                        pool,
                        memory,
                        &target,
                        EdgeOrigin::StructuralTagRef,
                        Some(format!("Tag {tag}")),
                    )
                    .await?;
                    count += 1;
                }
            }
        }
    }

    Ok(count)
}

async fn upsert_reference_edge(
    pool: &PgPool,
    source: &MemorySummary,
    target: &MemorySummary,
    origin: EdgeOrigin,
    evidence: Option<String>,
) -> Result<(), sqlx::Error> {
    let now = Utc::now();
    let edge = MemoryEdge {
        id: Uuid::new_v4(),
        confidence: 1.0,
        created_at: now,
        dst_id: target.id,
        dst_project: target.project.clone(),
        evidence,
        origin,
        relation: EdgeRelation::References,
        src_id: source.id,
        src_project: source.project.clone(),
        suppressed: false,
        updated_at: now,
        weight: 1.0,
    };
    crate::db::upsert_edge(pool, &edge).await?;
    Ok(())
}

/// Extract all valid UUIDs from a text string.
fn extract_uuids_from_text(text: &str) -> Vec<Uuid> {
    let mut uuids = Vec::new();
    // Simple regex-free approach: scan for UUID-shaped substrings.
    // UUIDs are 36 chars: 8-4-4-4-12
    for word in text.split(|c: char| !c.is_ascii_hexdigit() && c != '-') {
        if word.len() == 36
            && let Ok(uuid) = Uuid::parse_str(word)
            && !uuids.contains(&uuid)
        {
            uuids.push(uuid);
        }
    }
    uuids
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_uuids_empty() {
        assert!(extract_uuids_from_text("").is_empty());
    }

    #[test]
    fn extract_uuids_no_uuids() {
        assert!(extract_uuids_from_text("hello world, no uuids here").is_empty());
    }

    #[test]
    fn extract_uuids_single() {
        let text = "References memory 550e8400-e29b-41d4-a716-446655440000 for context.";
        let uuids = extract_uuids_from_text(text);
        assert_eq!(uuids.len(), 1);
        assert_eq!(
            uuids[0],
            Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap()
        );
    }

    #[test]
    fn extract_uuids_multiple() {
        let text =
            "See 550e8400-e29b-41d4-a716-446655440000 and 6ba7b810-9dad-11d1-80b4-00c04fd430c8.";
        let uuids = extract_uuids_from_text(text);
        assert_eq!(uuids.len(), 2);
    }

    #[test]
    fn extract_uuids_deduplicated() {
        let text = "550e8400-e29b-41d4-a716-446655440000 appears twice 550e8400-e29b-41d4-a716-446655440000";
        let uuids = extract_uuids_from_text(text);
        assert_eq!(uuids.len(), 1);
    }

    #[test]
    fn extract_uuids_in_context() {
        let text = "Plan ID: 550e8400-e29b-41d4-a716-446655440000\nReviewer: codex";
        let uuids = extract_uuids_from_text(text);
        assert_eq!(uuids.len(), 1);
    }

    #[test]
    fn is_in_scope_same_project() {
        let policy = ExpansionPolicy {
            cross_project: false,
            graph_hops: 1,
            include_general: false,
            project_allowlist: None,
            source_project: "myproject".to_owned(),
        };
        assert!(is_in_scope("myproject", &policy));
    }

    #[test]
    fn is_in_scope_general_denied_by_default() {
        let policy = ExpansionPolicy {
            cross_project: false,
            graph_hops: 1,
            include_general: false,
            project_allowlist: None,
            source_project: "myproject".to_owned(),
        };
        assert!(!is_in_scope("general", &policy));
    }

    #[test]
    fn is_in_scope_general_allowed_when_opted_in() {
        let policy = ExpansionPolicy {
            cross_project: false,
            graph_hops: 1,
            include_general: true,
            project_allowlist: None,
            source_project: "myproject".to_owned(),
        };
        assert!(is_in_scope("general", &policy));
    }

    #[test]
    fn is_in_scope_foreign_denied_by_default() {
        let policy = ExpansionPolicy {
            cross_project: false,
            graph_hops: 1,
            include_general: false,
            project_allowlist: None,
            source_project: "myproject".to_owned(),
        };
        assert!(!is_in_scope("other-project", &policy));
    }

    #[test]
    fn is_in_scope_foreign_allowed_with_cross_project() {
        let policy = ExpansionPolicy {
            cross_project: true,
            graph_hops: 1,
            include_general: false,
            project_allowlist: None,
            source_project: "myproject".to_owned(),
        };
        assert!(is_in_scope("other-project", &policy));
    }

    #[test]
    fn is_in_scope_foreign_filtered_by_allowlist() {
        let policy = ExpansionPolicy {
            cross_project: true,
            graph_hops: 1,
            include_general: false,
            project_allowlist: Some(vec!["allowed".to_owned()]),
            source_project: "myproject".to_owned(),
        };
        assert!(is_in_scope("allowed", &policy));
        assert!(!is_in_scope("blocked", &policy));
    }

    #[test]
    fn project_discount_values() {
        let policy = ExpansionPolicy {
            cross_project: true,
            graph_hops: 1,
            include_general: true,
            project_allowlist: None,
            source_project: "myproject".to_owned(),
        };
        assert!((project_discount("myproject", &policy) - 1.0).abs() < f64::EPSILON);
        assert!((project_discount("general", &policy) - GENERAL_DISCOUNT).abs() < f64::EPSILON);
        assert!(
            (project_discount("other-project", &policy) - FOREIGN_DISCOUNT).abs() < f64::EPSILON
        );
    }

    #[test]
    fn structural_tag_prefixes_sorted() {
        let mut sorted = STRUCTURAL_TAG_PREFIXES.to_vec();
        sorted.sort_unstable();
        assert_eq!(STRUCTURAL_TAG_PREFIXES, sorted.as_slice());
    }
}
