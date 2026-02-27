use std::fmt;

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, sqlx::Type)]
#[sqlx(type_name = "memory_category", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum Category {
    Context,
    Decision,
    ErrorFix,
    Plan,
    Rule,
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
