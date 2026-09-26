//! Strict, bounded publication of resolver-selected mandatory policies.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::Error;
use crate::policy::{CanonicalRule, DeliveryClass, PolicyWrite, ResolutionContext};

/// Maximum number of mandatory revisions in one snapshot.
pub const MAX_POLICIES: usize = 32;
/// Maximum UTF-8 bytes in the compact canonical digest input.
pub const MAX_CANONICAL_BYTES: usize = 16 * 1024;
/// Maximum UTF-8 bytes in a serialized pack or rendered publication.
pub const MAX_PUBLICATION_BYTES: usize = 24 * 1024;
/// Maximum UTF-8 bytes in an MCP tools/list response.
pub const MAX_TOOLS_LIST_BYTES: usize = 512 * 1024;
/// Maximum HTTP response body bytes accepted for guardrails.
pub const MAX_HTTP_BODY_BYTES: usize = 32 * 1024;

const DIGEST_DOMAIN: &str = "memory-guardrails-sha256-v1";

#[derive(Serialize)]
struct DigestInput<'a> {
    domain: &'static str,
    resolver_schema_version: u32,
    mandatory: &'a [CanonicalRule],
}

/// One immutable, scope-bound publication of exact mandatory rule text.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GuardrailPack {
    /// Publication schema version, currently 1.
    pub schema_version: u32,
    /// Explicitly selected work project.
    pub project: String,
    /// Trusted, normalized execution context.
    pub context: ResolutionContext,
    /// Authoritative resolver's canonical schema version.
    pub resolver_schema_version: u32,
    /// Complete mandatory policies in resolver order.
    pub mandatory: Vec<CanonicalRule>,
    /// Digest algorithm and canonical byte version.
    pub digest_algorithm: String,
    /// Lowercase SHA-256 of the canonical byte input, prefixed `sha256:`.
    pub digest: String,
}

impl GuardrailPack {
    /// Construct and validate a version-one pack from the authoritative resolver.
    ///
    /// # Errors
    /// Returns a typed policy error on empty, malformed, or excessive output.
    pub fn new(
        project: String,
        context: ResolutionContext,
        resolver_schema_version: u32,
        mandatory: Vec<CanonicalRule>,
    ) -> Result<Self, Error> {
        let mut pack = Self {
            schema_version: 1,
            project,
            context,
            resolver_schema_version,
            mandatory,
            digest_algorithm: DIGEST_DOMAIN.to_owned(),
            digest: String::new(),
        };
        pack.validate_shape()?;
        let bytes = pack.canonical_bytes()?;
        pack.digest = digest(&bytes);
        pack.check_serialized_size()?;
        pack.publication()?;
        Ok(pack)
    }

    /// Validate a decoded peer response against the immutable requested scope.
    ///
    /// # Errors
    /// Rejects missing, forged, reordered, or unsupported publications.
    pub fn validate_for(&self, project: &str, context: &ResolutionContext) -> Result<(), Error> {
        self.validate_shape()?;
        if self.project != project || &self.context != context {
            return Err(policy_error(
                "guardrails_scope_mismatch",
                "guardrail response does not match the configured project and context",
                serde_json::json!({"requested_project": project, "requested_context": context,
                    "received_project": self.project, "received_context": self.context}),
            ));
        }
        let bytes = self.canonical_bytes()?;
        let expected = digest(&bytes);
        if self.digest != expected {
            return Err(policy_error(
                "guardrails_malformed",
                "guardrail digest does not match the mandatory policies",
                serde_json::json!({"expected_digest": expected, "received_digest": self.digest}),
            ));
        }
        self.check_serialized_size()?;
        self.publication()?;
        Ok(())
    }

    /// Return the exact compact UTF-8 bytes covered by the digest.
    ///
    /// # Errors
    /// Returns a typed error if encoding or the canonical size limit fails.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, Error> {
        let bytes = serde_json::to_vec(&DigestInput {
            domain: DIGEST_DOMAIN,
            resolver_schema_version: self.resolver_schema_version,
            mandatory: &self.mandatory,
        })
        .map_err(|error| {
            policy_error(
                "guardrails_malformed",
                &error.to_string(),
                serde_json::json!({}),
            )
        })?;
        if bytes.len() > MAX_CANONICAL_BYTES {
            return Err(self.too_large("canonical_bytes", bytes.len(), MAX_CANONICAL_BYTES));
        }
        Ok(bytes)
    }

    /// Render a compact, self-contained publication for MCP instructions and tools.
    ///
    /// # Errors
    /// Returns a typed error if the publication exceeds its hard byte limit.
    pub fn publication(&self) -> Result<String, Error> {
        let context = serde_json::to_string(&self.context).map_err(|error| {
            policy_error(
                "guardrails_malformed",
                &error.to_string(),
                serde_json::json!({}),
            )
        })?;
        let mut text = format!(
            "Mandatory guardrails apply before state-changing work.\nWork project: {}\nContext: {}\nDigest: {}",
            self.project, context, self.digest,
        );
        for (index, rule) in self.mandatory.iter().enumerate() {
            let metadata = serde_json::to_string(&serde_json::json!({
                "project": rule.project, "id": rule.id, "policy_key": rule.policy_key,
                "revision": rule.revision, "delivery_class": rule.delivery_class,
                "selectors": rule.selectors, "values": rule.values,
                "overrides": rule.overrides,
            }))
            .map_err(|error| {
                policy_error(
                    "guardrails_malformed",
                    &error.to_string(),
                    serde_json::json!({}),
                )
            })?;
            write!(
                &mut text,
                "\nPolicy {}: {}\nExact content ({} UTF-8 bytes):\n",
                index + 1,
                metadata,
                rule.content.len()
            )
            .map_err(|error| {
                policy_error(
                    "guardrails_malformed",
                    &error.to_string(),
                    serde_json::json!({}),
                )
            })?;
            text.push_str(&rule.content);
            text.push('\n');
        }
        if text.len() > MAX_PUBLICATION_BYTES {
            return Err(self.too_large("publication_bytes", text.len(), MAX_PUBLICATION_BYTES));
        }
        Ok(text)
    }

    fn validate_shape(&self) -> Result<(), Error> {
        if self.schema_version != 1
            || self.resolver_schema_version != 1
            || self.digest_algorithm != DIGEST_DOMAIN
        {
            return Err(policy_error(
                "guardrails_unsupported_version",
                "unsupported guardrail pack, resolver, or digest version",
                serde_json::json!({"schema_version": self.schema_version,
                    "resolver_schema_version": self.resolver_schema_version,
                    "digest_algorithm": self.digest_algorithm}),
            ));
        }
        if self.project.trim().is_empty()
            || self.project != self.project.trim()
            || self.context.validate().is_err()
        {
            return Err(policy_error(
                "guardrails_malformed",
                "invalid guardrail scope",
                serde_json::json!({"project": self.project, "context": self.context}),
            ));
        }
        if self.mandatory.is_empty() {
            return Err(policy_error(
                "guardrails_empty",
                "no applicable classified mandatory policies; provision policies or check the configured binding",
                serde_json::json!({"project": self.project, "context": self.context}),
            ));
        }
        if self.mandatory.len() > MAX_POLICIES {
            return Err(self.too_large("policy_count", self.mandatory.len(), MAX_POLICIES));
        }
        let mut seen = BTreeSet::new();
        let mut previous = None;
        for rule in &self.mandatory {
            let (Some(key), Some(revision)) = (&rule.policy_key, rule.revision) else {
                return Err(policy_error(
                    "guardrails_malformed",
                    "mandatory policy lacks key or revision",
                    serde_json::json!({"id": rule.id}),
                ));
            };
            let metadata = PolicyWrite {
                policy_key: key.clone(),
                revision,
                delivery_class: DeliveryClass::Mandatory,
                supersedes: None,
                selectors: rule.selectors.clone(),
                values: rule.values.clone(),
            };
            if rule.delivery_class != Some(DeliveryClass::Mandatory)
                || rule.project.is_empty()
                || rule.id.is_nil()
                || rule.content.is_empty()
                || metadata.validate().is_err()
                || rule.overrides.as_ref().is_some_and(|reference| {
                    reference.project.is_empty()
                        || reference.id.is_nil()
                        || PolicyWrite {
                            policy_key: reference.policy_key.clone(),
                            revision: reference.revision,
                            delivery_class: reference.delivery_class,
                            supersedes: None,
                            selectors: reference.selectors.clone(),
                            values: BTreeMap::new(),
                        }
                        .validate()
                        .is_err()
                })
            {
                return Err(policy_error(
                    "guardrails_malformed",
                    "invalid mandatory policy identity or metadata",
                    serde_json::json!({"project": rule.project, "id": rule.id, "policy_key": key, "revision": revision}),
                ));
            }
            let order = (key.as_str(), rule.project.as_str(), revision, rule.id);
            if previous.as_ref().is_some_and(|prior| *prior >= order)
                || !seen.insert((rule.project.as_str(), key.as_str()))
            {
                return Err(policy_error(
                    "guardrails_malformed",
                    "duplicate or unordered mandatory policy",
                    serde_json::json!({"project": rule.project, "id": rule.id, "policy_key": key, "revision": revision}),
                ));
            }
            previous = Some(order);
        }
        Ok(())
    }

    fn check_serialized_size(&self) -> Result<(), Error> {
        let size = serde_json::to_vec(self)
            .map_err(|error| {
                policy_error(
                    "guardrails_malformed",
                    &error.to_string(),
                    serde_json::json!({}),
                )
            })?
            .len();
        if size > MAX_PUBLICATION_BYTES {
            return Err(self.too_large("pack_bytes", size, MAX_PUBLICATION_BYTES));
        }
        Ok(())
    }

    fn too_large(&self, measure: &str, actual: usize, allowed: usize) -> Error {
        policy_error(
            "guardrails_too_large",
            "mandatory guardrail pack exceeds a hard limit",
            serde_json::json!({
                "measure": measure, "actual": actual, "allowed": allowed,
                "policies": self.mandatory.iter().map(|rule| serde_json::json!({
                    "project": rule.project, "id": rule.id, "policy_key": rule.policy_key,
                    "revision": rule.revision
                })).collect::<Vec<_>>()
            }),
        )
    }
}

fn policy_error(code: &str, message: &str, details: serde_json::Value) -> Error {
    Error::Policy {
        code: code.to_owned(),
        message: message.to_owned(),
        details,
        conflict: false,
    }
}

fn digest(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(71);
    result.push_str("sha256:");
    for byte in Sha256::digest(bytes) {
        result.push(char::from(HEX[usize::from(byte >> 4)]));
        result.push(char::from(HEX[usize::from(byte & 15)]));
    }
    result
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use uuid::Uuid;

    use super::*;
    use crate::policy::{PolicyReference, PolicySelectors};

    fn rule(id: u128, key: &str, content: &str) -> CanonicalRule {
        CanonicalRule {
            project: "general".to_owned(),
            id: Uuid::from_u128(id),
            policy_key: Some(key.to_owned()),
            revision: Some(1),
            delivery_class: Some(DeliveryClass::Mandatory),
            selectors: PolicySelectors::default(),
            values: BTreeMap::new(),
            content: content.to_owned(),
            overrides: None,
        }
    }

    #[test]
    fn golden_v1_exact_bytes_and_digest() {
        let pack = GuardrailPack::new(
            "app".to_owned(),
            ResolutionContext::default(),
            1,
            vec![rule(10, "a", "Rüle \"exact\"\n\t🦀")],
        )
        .unwrap();
        let bytes = String::from_utf8(pack.canonical_bytes().unwrap()).unwrap();
        assert_eq!(
            bytes,
            "{\"domain\":\"memory-guardrails-sha256-v1\",\"resolver_schema_version\":1,\"mandatory\":[{\"project\":\"general\",\"id\":\"00000000-0000-0000-0000-00000000000a\",\"policy_key\":\"a\",\"revision\":1,\"delivery_class\":\"mandatory\",\"selectors\":{},\"values\":{},\"content\":\"Rüle \\\"exact\\\"\\n\\t🦀\",\"overrides\":null}]}"
        );
        assert_eq!(
            pack.digest,
            "sha256:ea75e758b5fa19e4b8447119597a753ee4eb86dab0d8877fc13c0f7fadb27c67"
        );
    }

    #[test]
    fn scope_is_outside_digest_and_normative_fields_change_it() {
        let base = rule(10, "a", "exact");
        let first = GuardrailPack::new(
            "app".to_owned(),
            ResolutionContext::default(),
            1,
            vec![base.clone()],
        )
        .unwrap();
        let second = GuardrailPack::new(
            "other".to_owned(),
            ResolutionContext {
                profile: Some("host".to_owned()),
                ..ResolutionContext::default()
            },
            1,
            vec![base.clone()],
        )
        .unwrap();
        assert_eq!(first.digest, second.digest);
        let mut variants = Vec::new();
        let mut revised = base.clone();
        revised.revision = Some(2);
        variants.push(revised);
        let mut changed_id = base.clone();
        changed_id.id = Uuid::from_u128(11);
        variants.push(changed_id);
        let mut changed_key = base.clone();
        changed_key.policy_key = Some("b".to_owned());
        variants.push(changed_key);
        let mut changed_project = base.clone();
        changed_project.project = "app".to_owned();
        variants.push(changed_project);
        let mut changed_selector = base.clone();
        changed_selector.selectors.profile = Some(["host".to_owned()].into());
        variants.push(changed_selector);
        let mut changed_value = base.clone();
        changed_value
            .values
            .insert("mode".to_owned(), "safe".to_owned());
        variants.push(changed_value);
        let mut changed_text = base.clone();
        changed_text.content.push('!');
        variants.push(changed_text);
        let mut changed_provenance = base;
        changed_provenance.project = "app".to_owned();
        changed_provenance.overrides = Some(PolicyReference {
            project: "general".to_owned(),
            id: Uuid::from_u128(20),
            policy_key: "a".to_owned(),
            revision: 1,
            delivery_class: DeliveryClass::Contextual,
            selectors: PolicySelectors::default(),
        });
        variants.push(changed_provenance);
        for variant in variants {
            let changed = GuardrailPack::new(
                "app".to_owned(),
                ResolutionContext::default(),
                1,
                vec![variant],
            )
            .unwrap();
            assert_ne!(first.digest, changed.digest);
        }
    }

    #[test]
    fn strict_shape_order_and_limits() {
        let context = ResolutionContext::default();
        let pack = GuardrailPack::new(
            "app".to_owned(),
            context.clone(),
            1,
            vec![rule(10, "a", "exact")],
        )
        .unwrap();
        assert!(pack.validate_for("other", &context).is_err());
        let mut forged = pack.clone();
        forged.digest = "sha256:deadbeef".to_owned();
        assert!(forged.validate_for("app", &context).is_err());
        let mut missing = serde_json::to_value(&pack).unwrap();
        missing.as_object_mut().unwrap().remove("mandatory");
        assert!(serde_json::from_value::<GuardrailPack>(missing).is_err());
        assert!(GuardrailPack::new("app".to_owned(), context.clone(), 1, vec![]).is_err());
        assert!(
            GuardrailPack::new(
                "app".to_owned(),
                context.clone(),
                1,
                (1..=32)
                    .map(|id| rule(id, &format!("a{id:02}"), "x"))
                    .collect()
            )
            .is_ok()
        );
        assert!(
            GuardrailPack::new(
                "app".to_owned(),
                context.clone(),
                1,
                vec![rule(1, "b", "x"), rule(2, "a", "x")]
            )
            .is_err()
        );
        let overhead = pack.canonical_bytes().unwrap().len() - "exact".len();
        let exact_text = "x".repeat(MAX_CANONICAL_BYTES - overhead);
        let exact = GuardrailPack::new(
            "app".to_owned(),
            context.clone(),
            1,
            vec![rule(10, "a", &exact_text)],
        )
        .unwrap();
        assert_eq!(exact.canonical_bytes().unwrap().len(), MAX_CANONICAL_BYTES);
        let over = format!("{exact_text}x");
        assert!(
            GuardrailPack::new(
                "app".to_owned(),
                context.clone(),
                1,
                vec![rule(10, "a", &over)]
            )
            .is_err()
        );
        assert!(
            GuardrailPack::new(
                "app".to_owned(),
                context.clone(),
                1,
                (1..=33)
                    .map(|id| rule(id, &format!("a{id:02}"), "x"))
                    .collect()
            )
            .is_err()
        );
        assert!(
            GuardrailPack::new(
                "app".to_owned(),
                context,
                1,
                vec![rule(1, "a", &"x".repeat(MAX_CANONICAL_BYTES))]
            )
            .is_err()
        );
    }

    #[test]
    fn set_insertion_order_does_not_change_digest() {
        let mut left = rule(10, "a", "exact");
        left.selectors.language = Some(
            ["rust".to_owned(), "go".to_owned()]
                .into_iter()
                .collect::<BTreeSet<_>>(),
        );
        left.values.insert("z".to_owned(), "last".to_owned());
        left.values.insert("a".to_owned(), "first".to_owned());
        let mut right = rule(10, "a", "exact");
        right.selectors.language = Some(
            ["go".to_owned(), "rust".to_owned()]
                .into_iter()
                .collect::<BTreeSet<_>>(),
        );
        right.values.insert("a".to_owned(), "first".to_owned());
        right.values.insert("z".to_owned(), "last".to_owned());
        let a = GuardrailPack::new(
            "app".to_owned(),
            ResolutionContext::default(),
            1,
            vec![left],
        )
        .unwrap();
        let b = GuardrailPack::new(
            "app".to_owned(),
            ResolutionContext::default(),
            1,
            vec![right],
        )
        .unwrap();
        assert_eq!(a.canonical_bytes().unwrap(), b.canonical_bytes().unwrap());
        assert_eq!(a.digest, b.digest);
    }
}
