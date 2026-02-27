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
}

impl fmt::Display for Category {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Context => write!(f, "context"),
            Self::Decision => write!(f, "decision"),
            Self::ErrorFix => write!(f, "error_fix"),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn category_display() {
        assert_eq!(Category::Context.to_string(), "context");
        assert_eq!(Category::Decision.to_string(), "decision");
        assert_eq!(Category::ErrorFix.to_string(), "error_fix");
    }

    #[test]
    fn category_serde_roundtrip() {
        let json = serde_json::to_string(&Category::ErrorFix).unwrap();
        assert_eq!(json, r#""error_fix""#);
        let parsed: Category = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, Category::ErrorFix);
    }

    #[test]
    fn category_alphabetical_order() {
        let variants = [Category::Context, Category::Decision, Category::ErrorFix];
        let names: Vec<&str> = variants
            .iter()
            .map(|c| match c {
                Category::Context => "context",
                Category::Decision => "decision",
                Category::ErrorFix => "error_fix",
            })
            .collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted);
    }
}
