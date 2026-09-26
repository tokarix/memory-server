//! Stable identity and publication metadata for classified Rule memories.

use chrono::{DateTime, SecondsFormat, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Whether a policy is contextual guidance or requires later mandatory delivery.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryClass {
    /// Delivered only through the existing contextual rule filters.
    Contextual,
    /// Marked for mandatory delivery by a future policy resolver.
    Mandatory,
}

/// Publication state controlled by the server.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyState {
    /// The current revision for its project and key.
    Active,
    /// A later revision explicitly replaced this revision.
    Superseded,
}

/// Complete client supplied metadata for a new policy revision.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyWrite {
    /// Stable key within the memory's project; 1–128 ASCII bytes.
    #[schemars(regex(pattern = "^[a-z0-9][a-z0-9._:-]{0,127}$"))]
    pub policy_key: String,
    /// Positive revision number, stored in a signed 64-bit database column.
    #[schemars(range(min = 1))]
    pub revision: i64,
    /// Classification of this revision.
    pub delivery_class: DeliveryClass,
    /// Current active predecessor, omitted for a root revision.
    pub supersedes: Option<Uuid>,
}

impl PolicyWrite {
    /// Validate fields that cannot be expressed by Serde's shape checks.
    ///
    /// # Errors
    ///
    /// Returns a description when the key or revision is invalid.
    pub fn validate(&self) -> Result<(), &'static str> {
        let bytes = self.policy_key.as_bytes();
        if bytes.is_empty()
            || bytes.len() > 128
            || !matches!(bytes[0], b'a'..=b'z' | b'0'..=b'9')
            || !bytes[1..]
                .iter()
                .all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b':' | b'-'))
        {
            return Err("policy_key must match ^[a-z0-9][a-z0-9._:-]{0,127}$");
        }
        if self.revision <= 0 {
            return Err("policy revision must be a positive signed 64-bit integer");
        }
        Ok(())
    }
}

/// Persisted metadata returned on a classified Rule memory.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct PolicyMetadata {
    /// Stable key within the memory's project.
    pub policy_key: String,
    /// Positive revision number.
    pub revision: i64,
    /// Delivery classification of this immutable revision.
    pub delivery_class: DeliveryClass,
    /// Current or historical publication state.
    pub state: PolicyState,
    /// Explicit predecessor UUID, if any.
    pub supersedes: Option<Uuid>,
}

/// Format the full persisted instant for MCP read-then-assign operations.
#[must_use]
pub fn format_updated_at(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_and_revision_validation() {
        for key in ["a", "0", "build.storage", "a_b:c-d"] {
            assert!(
                PolicyWrite {
                    policy_key: key.into(),
                    revision: 1,
                    delivery_class: DeliveryClass::Contextual,
                    supersedes: None,
                }
                .validate()
                .is_ok()
            );
        }
        for key in ["", "A", "-a", "a/b", "a b", "é", &"a".repeat(129)] {
            assert!(
                PolicyWrite {
                    policy_key: key.into(),
                    revision: 1,
                    delivery_class: DeliveryClass::Contextual,
                    supersedes: None,
                }
                .validate()
                .is_err()
            );
        }
        assert!(
            PolicyWrite {
                policy_key: "a".into(),
                revision: 0,
                delivery_class: DeliveryClass::Contextual,
                supersedes: None,
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn rejects_server_owned_and_unknown_write_fields() {
        let json =
            r#"{"policy_key":"a","revision":1,"delivery_class":"mandatory","state":"active"}"#;
        assert!(serde_json::from_str::<PolicyWrite>(json).is_err());
        for json in [
            r#"{"policy_key":"a","revision":1}"#,
            r#"{"policy_key":"a","revision":1,"delivery_class":"critical"}"#,
            r#"{"policy_key":"a","revision":9223372036854775808,"delivery_class":"mandatory"}"#,
            r#"{"policy_key":"a","revision":1,"delivery_class":"mandatory","other":true}"#,
        ] {
            assert!(serde_json::from_str::<PolicyWrite>(json).is_err(), "{json}");
        }
    }

    #[test]
    fn canonical_time_keeps_microseconds() {
        let value = DateTime::parse_from_rfc3339("2026-09-26T12:34:27.123456Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(format_updated_at(value), "2026-09-26T12:34:27.123456000Z");
    }

    #[test]
    fn read_metadata_has_fixed_field_order() {
        let metadata = PolicyMetadata {
            policy_key: "build.storage".to_owned(),
            revision: 2,
            delivery_class: DeliveryClass::Mandatory,
            state: PolicyState::Active,
            supersedes: None,
        };
        assert_eq!(
            serde_json::to_string(&metadata).unwrap(),
            r#"{"policy_key":"build.storage","revision":2,"delivery_class":"mandatory","state":"active","supersedes":null}"#
        );
    }
}
