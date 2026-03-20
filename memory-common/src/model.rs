use std::fmt;

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[cfg_attr(feature = "sqlx", derive(sqlx::Type))]
#[cfg_attr(
    feature = "sqlx",
    sqlx(type_name = "memory_category", rename_all = "snake_case")
)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    Context,
    Decision,
    ErrorFix,
    Plan,
    Rule,
}

impl Category {
    #[must_use]
    pub fn importance(&self) -> f64 {
        match self {
            Self::Context => 0.5,
            Self::Decision => 0.75,
            Self::ErrorFix => 0.7,
            Self::Plan => 0.85,
            Self::Rule => 0.9,
        }
    }

    /// Returns `true` if this category is considered "core" (importance >= 0.7).
    #[must_use]
    pub fn is_core(&self) -> bool {
        self.importance() >= 0.7
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[cfg_attr(feature = "sqlx", derive(sqlx::Type))]
#[cfg_attr(
    feature = "sqlx",
    sqlx(type_name = "edge_origin", rename_all = "snake_case")
)]
#[serde(rename_all = "snake_case")]
pub enum EdgeOrigin {
    ContentUuidRef,
    EmbeddingNeighbor,
    Manual,
    SharedTag,
    StructuralTagRef,
    UsageReinforcement,
}

impl fmt::Display for EdgeOrigin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ContentUuidRef => write!(f, "content_uuid_ref"),
            Self::EmbeddingNeighbor => write!(f, "embedding_neighbor"),
            Self::Manual => write!(f, "manual"),
            Self::SharedTag => write!(f, "shared_tag"),
            Self::StructuralTagRef => write!(f, "structural_tag_ref"),
            Self::UsageReinforcement => write!(f, "usage_reinforcement"),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[cfg_attr(feature = "sqlx", derive(sqlx::Type))]
#[cfg_attr(
    feature = "sqlx",
    sqlx(type_name = "edge_relation", rename_all = "snake_case")
)]
#[serde(rename_all = "snake_case")]
pub enum EdgeRelation {
    References,
    RelatedTag,
    Similar,
}

impl fmt::Display for EdgeRelation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::References => write!(f, "references"),
            Self::RelatedTag => write!(f, "related_tag"),
            Self::Similar => write!(f, "similar"),
        }
    }
}

impl fmt::Display for Category {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Context => write!(f, "context"),
            Self::Decision => write!(f, "decision"),
            Self::ErrorFix => write!(f, "error_fix"),
            Self::Plan => write!(f, "plan"),
            Self::Rule => write!(f, "rule"),
        }
    }
}

pub struct Memory {
    pub id: Uuid,
    pub category: Category,
    pub content: String,
    pub created_at: DateTime<Utc>,
    pub embedding: Vec<f32>,
    pub project: String,
    pub summary: String,
    pub tags: Vec<String>,
    pub updated_at: DateTime<Utc>,
}

/// Memory without the embedding vector, for list/get queries.
#[derive(Clone)]
pub struct MemorySummary {
    pub id: Uuid,
    pub category: Category,
    pub content: String,
    pub created_at: DateTime<Utc>,
    pub project: String,
    pub summary: String,
    pub tags: Vec<String>,
    pub updated_at: DateTime<Utc>,
}

pub struct MemoryEdge {
    pub id: Uuid,
    pub confidence: f64,
    pub created_at: DateTime<Utc>,
    pub dst_id: Uuid,
    pub dst_project: String,
    pub evidence: Option<String>,
    pub origin: EdgeOrigin,
    pub relation: EdgeRelation,
    pub src_id: Uuid,
    pub src_project: String,
    pub suppressed: bool,
    pub updated_at: DateTime<Utc>,
    pub weight: f64,
}

/// Edge without full metadata, for list/inspection queries.
#[derive(Clone)]
pub struct MemoryEdgeSummary {
    pub id: Uuid,
    pub confidence: f64,
    pub created_at: DateTime<Utc>,
    pub dst_id: Uuid,
    pub dst_project: String,
    pub evidence: Option<String>,
    pub origin: EdgeOrigin,
    pub relation: EdgeRelation,
    pub src_id: Uuid,
    pub src_project: String,
    pub suppressed: bool,
    pub updated_at: DateTime<Utc>,
    pub weight: f64,
}

pub struct SessionLog {
    pub id: Uuid,
    pub content: String,
    pub created_at: DateTime<Utc>,
    pub cwd: String,
    pub embedding: Vec<f32>,
    pub project: String,
    pub session_id: String,
    pub summary: String,
}

pub struct SessionLogChunk {
    pub chunk_index: i32,
    pub content: String,
    pub embedding: Vec<f32>,
    pub id: Uuid,
    pub session_log_id: Uuid,
}

pub struct Session {
    pub agent: String,
    pub created_at: DateTime<Utc>,
    pub cwd: String,
    pub ended_at: Option<DateTime<Utc>>,
    pub external_session_id: String,
    pub id: Uuid,
    pub project: String,
    pub updated_at: DateTime<Utc>,
}

pub struct SessionMessage {
    pub agent: String,
    pub content: String,
    pub created_at: DateTime<Utc>,
    pub id: Uuid,
    pub kind: String,
    pub metadata: Option<String>,
    pub role: String,
    pub session_id: Uuid,
}

/// Session log without the embedding vector, for search results.
#[derive(Clone)]
pub struct SessionLogSummary {
    pub id: Uuid,
    pub content: String,
    pub created_at: DateTime<Utc>,
    pub cwd: String,
    pub project: String,
    pub session_id: String,
    pub summary: String,
}

#[derive(Clone)]
pub struct SessionSummary {
    pub agent: String,
    pub created_at: DateTime<Utc>,
    pub cwd: String,
    pub ended_at: Option<DateTime<Utc>>,
    pub external_session_id: String,
    pub id: Uuid,
    pub project: String,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone)]
pub struct SessionMessageSummary {
    pub agent: String,
    pub content: String,
    pub created_at: DateTime<Utc>,
    pub id: Uuid,
    pub kind: String,
    pub metadata: Option<String>,
    pub role: String,
    pub session_id: Uuid,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn category_display() {
        assert_eq!(Category::Context.to_string(), "context");
        assert_eq!(Category::Decision.to_string(), "decision");
        assert_eq!(Category::ErrorFix.to_string(), "error_fix");
        assert_eq!(Category::Plan.to_string(), "plan");
        assert_eq!(Category::Rule.to_string(), "rule");
    }

    #[test]
    fn category_serde_roundtrip() {
        for (variant, expected) in [
            (Category::Context, r#""context""#),
            (Category::Decision, r#""decision""#),
            (Category::ErrorFix, r#""error_fix""#),
            (Category::Plan, r#""plan""#),
            (Category::Rule, r#""rule""#),
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected);
            let parsed: Category = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, variant);
        }
    }

    #[test]
    fn category_importance_values() {
        assert!((Category::Context.importance() - 0.5).abs() < f64::EPSILON);
        assert!((Category::Decision.importance() - 0.75).abs() < f64::EPSILON);
        assert!((Category::ErrorFix.importance() - 0.7).abs() < f64::EPSILON);
        assert!((Category::Plan.importance() - 0.85).abs() < f64::EPSILON);
        assert!((Category::Rule.importance() - 0.9).abs() < f64::EPSILON);
    }

    #[test]
    fn category_importance_in_range() {
        let variants = [
            Category::Context,
            Category::Decision,
            Category::ErrorFix,
            Category::Plan,
            Category::Rule,
        ];
        for variant in &variants {
            let imp = variant.importance();
            assert!(
                (0.0..=1.0).contains(&imp),
                "{variant} importance {imp} out of range"
            );
        }
    }

    #[test]
    fn category_is_core() {
        assert!(!Category::Context.is_core());
        assert!(Category::Decision.is_core());
        assert!(Category::ErrorFix.is_core());
        assert!(Category::Plan.is_core());
        assert!(Category::Rule.is_core());
    }

    #[test]
    fn edge_origin_display() {
        assert_eq!(EdgeOrigin::ContentUuidRef.to_string(), "content_uuid_ref");
        assert_eq!(
            EdgeOrigin::EmbeddingNeighbor.to_string(),
            "embedding_neighbor"
        );
        assert_eq!(EdgeOrigin::Manual.to_string(), "manual");
        assert_eq!(EdgeOrigin::SharedTag.to_string(), "shared_tag");
        assert_eq!(
            EdgeOrigin::StructuralTagRef.to_string(),
            "structural_tag_ref"
        );
        assert_eq!(
            EdgeOrigin::UsageReinforcement.to_string(),
            "usage_reinforcement"
        );
    }

    #[test]
    fn edge_origin_serde_roundtrip() {
        for (variant, expected) in [
            (EdgeOrigin::ContentUuidRef, r#""content_uuid_ref""#),
            (EdgeOrigin::EmbeddingNeighbor, r#""embedding_neighbor""#),
            (EdgeOrigin::Manual, r#""manual""#),
            (EdgeOrigin::SharedTag, r#""shared_tag""#),
            (EdgeOrigin::StructuralTagRef, r#""structural_tag_ref""#),
            (EdgeOrigin::UsageReinforcement, r#""usage_reinforcement""#),
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected);
            let parsed: EdgeOrigin = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, variant);
        }
    }

    #[test]
    fn edge_origin_alphabetical_order() {
        let names: Vec<&str> = [
            EdgeOrigin::ContentUuidRef,
            EdgeOrigin::EmbeddingNeighbor,
            EdgeOrigin::Manual,
            EdgeOrigin::SharedTag,
            EdgeOrigin::StructuralTagRef,
            EdgeOrigin::UsageReinforcement,
        ]
        .iter()
        .map(|o| match o {
            EdgeOrigin::ContentUuidRef => "content_uuid_ref",
            EdgeOrigin::EmbeddingNeighbor => "embedding_neighbor",
            EdgeOrigin::Manual => "manual",
            EdgeOrigin::SharedTag => "shared_tag",
            EdgeOrigin::StructuralTagRef => "structural_tag_ref",
            EdgeOrigin::UsageReinforcement => "usage_reinforcement",
        })
        .collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted);
    }

    #[test]
    fn edge_relation_display() {
        assert_eq!(EdgeRelation::References.to_string(), "references");
        assert_eq!(EdgeRelation::RelatedTag.to_string(), "related_tag");
        assert_eq!(EdgeRelation::Similar.to_string(), "similar");
    }

    #[test]
    fn edge_relation_serde_roundtrip() {
        for (variant, expected) in [
            (EdgeRelation::References, r#""references""#),
            (EdgeRelation::RelatedTag, r#""related_tag""#),
            (EdgeRelation::Similar, r#""similar""#),
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected);
            let parsed: EdgeRelation = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, variant);
        }
    }

    #[test]
    fn edge_relation_alphabetical_order() {
        let names: Vec<&str> = [
            EdgeRelation::References,
            EdgeRelation::RelatedTag,
            EdgeRelation::Similar,
        ]
        .iter()
        .map(|r| match r {
            EdgeRelation::References => "references",
            EdgeRelation::RelatedTag => "related_tag",
            EdgeRelation::Similar => "similar",
        })
        .collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted);
    }

    #[test]
    fn category_alphabetical_order() {
        let variants = [
            Category::Context,
            Category::Decision,
            Category::ErrorFix,
            Category::Plan,
            Category::Rule,
        ];
        let names: Vec<&str> = variants
            .iter()
            .map(|c| match c {
                Category::Context => "context",
                Category::Decision => "decision",
                Category::ErrorFix => "error_fix",
                Category::Plan => "plan",
                Category::Rule => "rule",
            })
            .collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted);
    }
}
