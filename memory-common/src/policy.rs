//! Stable identity and publication metadata for classified Rule memories.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, SecondsFormat, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};
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

/// Structured execution domain of a policy. An omitted dimension is universal.
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PolicySelectors {
    /// Execution target profiles accepted by this policy.
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub profile: Option<BTreeSet<String>>,
    /// Work phases accepted by this policy.
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub phase: Option<BTreeSet<String>>,
    /// Source languages accepted by this policy.
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub language: Option<BTreeSet<String>>,
    /// Tools accepted by this policy.
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub tool: Option<BTreeSet<String>>,
}

/// Trusted execution facts. Missing and explicitly empty sets have different meanings.
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResolutionContext {
    /// The execution target, such as `workstation-host` or `woodpecker-container`.
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub profile: Option<String>,
    /// Current work phase, if asserted by orchestration.
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub phase: Option<String>,
    /// Known languages, including an empty set when none apply.
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub language: Option<BTreeSet<String>>,
    /// Known tools, including an empty set when none apply.
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub tool: Option<BTreeSet<String>>,
}

fn present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

fn valid_identifier(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 128
        && matches!(bytes[0], b'a'..=b'z' | b'0'..=b'9')
        && bytes[1..]
            .iter()
            .all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b':' | b'-'))
}

impl PolicySelectors {
    /// Validate bounded selector dimensions and canonical identifiers.
    ///
    /// # Errors
    /// Returns a description of an invalid dimension.
    pub fn validate(&self) -> Result<(), &'static str> {
        for values in [&self.profile, &self.phase, &self.language, &self.tool]
            .into_iter()
            .flatten()
        {
            if values.is_empty() || values.len() > 32 || !values.iter().all(|s| valid_identifier(s))
            {
                return Err("selector dimensions need 1..32 canonical identifiers");
            }
        }
        Ok(())
    }

    /// Return missing context fields, or an empty vector for a known mismatch.
    #[must_use]
    pub fn missing_for(&self, context: &ResolutionContext) -> Option<Vec<&'static str>> {
        let mut missing = Vec::new();
        for (name, selector, known) in [
            ("profile", &self.profile, &context.profile),
            ("phase", &self.phase, &context.phase),
        ] {
            if let Some(selector) = selector {
                match known {
                    Some(value) if !selector.contains(value) => return None,
                    None => missing.push(name),
                    _ => {}
                }
            }
        }
        for (name, selector, known) in [
            ("language", &self.language, &context.language),
            ("tool", &self.tool, &context.tool),
        ] {
            if let Some(selector) = selector {
                match known {
                    Some(values) if selector.is_disjoint(values) => return None,
                    None => missing.push(name),
                    _ => {}
                }
            }
        }
        Some(missing)
    }

    /// Whether every execution matched by `self` is also matched by `other`.
    #[must_use]
    pub fn is_subset_of(&self, other: &Self) -> bool {
        [
            (&self.profile, &other.profile),
            (&self.phase, &other.phase),
            (&self.language, &other.language),
            (&self.tool, &other.tool),
        ]
        .into_iter()
        .all(|(left, right)| match (left, right) {
            (_, None) => true,
            (Some(left), Some(right)) => left.is_subset(right),
            (None, Some(_)) => false,
        })
    }

    /// Whether the two selector domains share at least one execution.
    #[must_use]
    pub fn intersects(&self, other: &Self) -> bool {
        [
            (&self.profile, &other.profile),
            (&self.phase, &other.phase),
            (&self.language, &other.language),
            (&self.tool, &other.tool),
        ]
        .into_iter()
        .all(|(left, right)| match (left, right) {
            (Some(left), Some(right)) => !left.is_disjoint(right),
            _ => true,
        })
    }
}

impl ResolutionContext {
    /// Validate identifiers supplied by the trusted caller.
    ///
    /// # Errors
    /// Returns a description of malformed or excessive context data.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self
            .profile
            .as_deref()
            .is_some_and(|s| !valid_identifier(s))
            || self.phase.as_deref().is_some_and(|s| !valid_identifier(s))
        {
            return Err("context scalars must be canonical identifiers");
        }
        for values in [&self.language, &self.tool].into_iter().flatten() {
            if values.len() > 32 || !values.iter().all(|s| valid_identifier(s)) {
                return Err("context sets need at most 32 canonical identifiers");
            }
        }
        Ok(())
    }
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
    /// Structured execution domain; omitted dimensions match all executions.
    #[serde(default)]
    pub selectors: PolicySelectors,
    /// Exact requirements shared with other effective policies.
    #[serde(default)]
    pub values: BTreeMap<String, String>,
}

impl PolicyWrite {
    /// Validate fields that cannot be expressed by Serde's shape checks.
    ///
    /// # Errors
    ///
    /// Returns a description when the key or revision is invalid.
    pub fn validate(&self) -> Result<(), &'static str> {
        if !valid_identifier(&self.policy_key) {
            return Err("policy_key must match ^[a-z0-9][a-z0-9._:-]{0,127}$");
        }
        if self.revision <= 0 {
            return Err("policy revision must be a positive signed 64-bit integer");
        }
        self.selectors.validate()?;
        if self.values.len() > 64
            || self.values.iter().any(|(key, value)| {
                !valid_identifier(key)
                    || value.is_empty()
                    || value.len() > 1024
                    || value.trim() != value
            })
        {
            return Err("values need at most 64 canonical keys and nonblank 1..1024-byte strings");
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
    /// Structured execution domain.
    #[serde(default)]
    pub selectors: PolicySelectors,
    /// Declared exact setting requirements.
    #[serde(default)]
    pub values: BTreeMap<String, String>,
}

/// Stable identity used in resolution diagnostics and override provenance.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyReference {
    /// Project that owns the revision.
    pub project: String,
    /// Immutable memory identifier.
    pub id: Uuid,
    /// Stable identity key.
    pub policy_key: String,
    /// Explicit revision number.
    pub revision: i64,
    /// Delivery classification.
    pub delivery_class: DeliveryClass,
    /// Exact execution domain.
    pub selectors: PolicySelectors,
}

/// One canonical effective rule, free of timestamps and retrieval scores.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CanonicalRule {
    /// Source project.
    pub project: String,
    /// Immutable UUID.
    pub id: Uuid,
    /// Key and revision are absent only for a legacy Rule.
    pub policy_key: Option<String>,
    /// Explicit revision, when classified.
    pub revision: Option<i64>,
    /// Delivery class, when classified.
    pub delivery_class: Option<DeliveryClass>,
    /// Structured selectors; empty for legacy Rules.
    pub selectors: PolicySelectors,
    /// Declared setting requirements.
    pub values: BTreeMap<String, String>,
    /// Exact normative content.
    pub content: String,
    /// General policy replaced by this project policy, if any.
    pub overrides: Option<PolicyReference>,
}

/// Ordered, versioned policy representation for a future guardrails consumer.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CanonicalPolicySet {
    /// Canonical schema version.
    pub schema_version: u32,
    /// All effective rules in canonical order.
    pub effective: Vec<CanonicalRule>,
    /// Mandatory subset, in the same order.
    pub mandatory: Vec<CanonicalRule>,
}

/// Request choices recorded separately from the canonical policy bytes.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ResolutionOptions {
    /// Whether optional general contextual guidance participates.
    pub include_general: bool,
    /// Whether safe same-key project shadowing is enabled.
    pub shadow_general: bool,
    /// ALL-of contextual tag filter, if supplied.
    pub tags: Option<Vec<String>>,
}

impl Default for ResolutionOptions {
    fn default() -> Self {
        Self {
            include_general: true,
            shadow_general: true,
            tags: None,
        }
    }
}

impl Default for CanonicalPolicySet {
    fn default() -> Self {
        Self {
            schema_version: 1,
            effective: Vec::new(),
            mandatory: Vec::new(),
        }
    }
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
                    selectors: PolicySelectors::default(),
                    values: BTreeMap::new(),
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
                    selectors: PolicySelectors::default(),
                    values: BTreeMap::new(),
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
                selectors: PolicySelectors::default(),
                values: BTreeMap::new(),
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
            selectors: PolicySelectors::default(),
            values: BTreeMap::new(),
        };
        assert_eq!(
            serde_json::to_string(&metadata).unwrap(),
            r#"{"policy_key":"build.storage","revision":2,"delivery_class":"mandatory","state":"active","supersedes":null,"selectors":{},"values":{}}"#
        );
    }

    #[test]
    fn selector_json_is_strict_and_canonical() {
        for json in [
            r#"{"profile":null}"#,
            r#"{"profile":[]}"#,
            r#"{"unknown":["host"]}"#,
        ] {
            if let Ok(selectors) = serde_json::from_str::<PolicySelectors>(json) {
                assert!(selectors.validate().is_err(), "{json}");
            }
        }
        let selectors: PolicySelectors = serde_json::from_str(
            r#"{"profile":["woodpecker-container","workstation-host","workstation-host"]}"#,
        )
        .unwrap();
        assert_eq!(
            serde_json::to_string(&selectors).unwrap(),
            r#"{"profile":["woodpecker-container","workstation-host"]}"#
        );
    }

    #[test]
    fn subset_and_intersection_are_semantic() {
        let broad = PolicySelectors::default();
        let narrow = PolicySelectors {
            profile: Some(["workstation-host".to_owned()].into()),
            ..PolicySelectors::default()
        };
        let disjoint = PolicySelectors {
            profile: Some(["woodpecker-container".to_owned()].into()),
            ..PolicySelectors::default()
        };
        assert!(narrow.is_subset_of(&broad));
        assert!(!broad.is_subset_of(&narrow));
        assert!(!narrow.intersects(&disjoint));
        assert!(broad.intersects(&disjoint));
    }

    #[test]
    fn context_distinguishes_unknown_and_known_empty() {
        let selectors = PolicySelectors {
            language: Some(["rust".to_owned()].into()),
            ..PolicySelectors::default()
        };
        assert_eq!(
            selectors.missing_for(&ResolutionContext::default()),
            Some(vec!["language"])
        );
        let context = ResolutionContext {
            language: Some(BTreeSet::new()),
            ..ResolutionContext::default()
        };
        assert_eq!(selectors.missing_for(&context), None);
        assert_eq!(
            serde_json::to_string(&context).unwrap(),
            r#"{"language":[]}"#
        );
    }
}
