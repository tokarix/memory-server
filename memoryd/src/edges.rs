use chrono::Utc;
use sqlx::PgPool;
use uuid::Uuid;

use crate::model::{EdgeOrigin, EdgeRelation, MemoryEdge, MemorySummary};

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
    fn structural_tag_prefixes_sorted() {
        let mut sorted = STRUCTURAL_TAG_PREFIXES.to_vec();
        sorted.sort_unstable();
        assert_eq!(STRUCTURAL_TAG_PREFIXES, sorted.as_slice());
    }
}
