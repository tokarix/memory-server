//! Mandatory anchor-relative, redacted audit evidence for guarded events.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::config::{ClientAdapter, DelegationConfig, GateConfig, hash_part};
use crate::error::{Error, Result};
use crate::identity::ResolvedBinding;
use crate::installation::Installation;
use crate::pre_tool::{PreToolEvent, ToolCategory};
use crate::session_identity::ChildIdentity;
use crate::snapshot::{ClaimedCheck, session_hash};
use crate::storage::{AuditProjection, Outcome, Report, scope_digest};

const COUNTER: &str = "audit-counter";
const LOCK: &str = "lock-audit";

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Counter {
    version: u32,
    installation_id: Uuid,
    records: u32,
    bytes: u32,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u32,
    event_id: Uuid,
    attempt: Uuid,
    adapter: ClientAdapter,
    category: ToolCategory,
    session_hash: Option<String>,
    binding_hash: Option<String>,
    epoch: Option<u64>,
    generation: Option<Uuid>,
    intent: String,
    reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    event_kind: Option<AuditEvent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parent_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    child_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rust: Option<RustEvidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    linkage: Option<String>,
}

/// Safe classification persisted without execution data.
#[derive(Clone, Copy, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RustOutcome {
    AuditedReadOnly,
    CompleteBuild,
    Indeterminate,
}

/// Validated redacted analysis handoff. Paths and command values cannot enter it.
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RustEvidence {
    analysis: RustOutcome,
    candidates: u8,
    contract: u8,
    operation: String,
    scope: String,
    pack: String,
    storage: Option<AuditProjection>,
}

impl RustEvidence {
    pub(crate) fn new(
        binding: &ResolvedBinding,
        claim: &ClaimedCheck,
        operation: &[u8],
        contract: u8,
    ) -> Result<Self> {
        claim
            .pack
            .validate_for(binding.project(), binding.context())
            .map_err(|_| Error::StateScopeMismatch)?;
        let mut hash = Sha256::new();
        hash.update(b"memory-hooks-rust-audit-operation-v1\0");
        hash_part(&mut hash, operation);
        let evidence = Self {
            analysis: RustOutcome::Indeterminate,
            candidates: 0,
            contract,
            operation: format!("{:x}", hash.finalize()),
            scope: scope_digest(binding).map_err(|_| Error::StateCorrupt)?,
            pack: claim.pack.digest.clone(),
            storage: None,
        };
        evidence.validate()?;
        Ok(evidence)
    }

    pub(crate) fn analyzed(&mut self, outcome: RustOutcome, count: usize) -> Result<()> {
        self.analysis = outcome;
        self.candidates = u8::try_from(count).map_err(|_| Error::StateCorrupt)?;
        self.validate()
    }

    pub(crate) fn attach(&mut self, report: &Report) -> Result<()> {
        if report.scope_hash() != self.scope || report.pack_digest() != self.pack {
            return Err(Error::StateScopeMismatch);
        }
        self.storage = Some(report.audit_projection().map_err(|_| Error::StateCorrupt)?);
        self.validate()
    }

    fn validate(&self) -> Result<()> {
        if !matches!(self.contract, 0 | 2 | 3)
            || !valid_hash(&self.operation)
            || !valid_hash(&self.scope)
            || self.pack.len() != 71
            || !self.pack.starts_with("sha256:")
            || !valid_hash(&self.pack[7..])
            || self.candidates > 64
            || match self.analysis {
                RustOutcome::CompleteBuild => {
                    !matches!(self.contract, 2 | 3) || self.candidates < 4
                }
                RustOutcome::AuditedReadOnly => {
                    !matches!(self.contract, 2 | 3)
                        || self.candidates != 0
                        || self.storage.is_some()
                }
                RustOutcome::Indeterminate => self.candidates != 0 || self.storage.is_some(),
            }
        {
            return Err(Error::StateCorrupt);
        }
        if let Some(projection) = &self.storage {
            projection.validate().map_err(|_| Error::StateCorrupt)?;
            if projection
                .candidate_index()
                .is_some_and(|index| index >= self.candidates)
                || projection.counts().iter().copied().sum::<u32>()
                    > u32::from(self.candidates) * 64
                || projection.outcome() == Outcome::NotApplicable
            {
                return Err(Error::StateCorrupt);
            }
        }
        Ok(())
    }
}

fn linkage(record: &Record) -> Result<String> {
    // Serialize with the linkage omitted; all claim and projection fields enter
    // the domain-separated digest. Hashes are evidence, not secret storage.
    let mut value = serde_json::to_value(record).map_err(|_| Error::StateCorrupt)?;
    value
        .as_object_mut()
        .ok_or(Error::StateCorrupt)?
        .remove("linkage");
    let bytes = serde_json::to_vec(&value).map_err(|_| Error::StateCorrupt)?;
    let mut hash = Sha256::new();
    hash.update(b"memory-hooks-rust-audit-linkage-v1\0");
    hash_part(&mut hash, &bytes);
    Ok(format!("{:x}", hash.finalize()))
}

#[derive(Clone, Copy, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum AuditEvent {
    ChildStart,
    ChildPreTool,
    ParentSpawn,
}

fn name(index: u32) -> String {
    format!("audit-{index:08}")
}

fn binding_hash(binding: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"memory-hooks-audit-binding-v1\0");
    hash_part(&mut hasher, binding.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn valid_reason(reason: &str) -> bool {
    matches!(
        reason,
        "checked"
            | "published"
            | "policy_changed"
            | "guardrails_unavailable"
            | "snapshot_invalid"
            | "scope_mismatch"
            | "audit_failed"
            | "fresh_session_required"
            | "event_invalid"
            | "gate_contract_required"
            | "unsupported_capability"
            | "missing_child_linkage"
            | "parent_invalid"
            | "rust_analysis_indeterminate"
            | "rust_execution_unsupported"
            | "rust_storage_denied"
            | "rust_storage_indeterminate"
    )
}

/// Persist bounded child publication evidence before Emitted becomes usable.
pub(crate) fn append_child_start(
    installation: &Installation,
    child_hash: &str,
    parent_hash: &str,
    binding: &str,
    epoch: u64,
    generation: Uuid,
    limits: GateConfig,
) -> Result<()> {
    let record = Record {
        version: 2,
        event_id: Uuid::new_v4(),
        attempt: Uuid::new_v4(),
        adapter: installation.client(),
        category: ToolCategory::Agent,
        session_hash: None,
        binding_hash: Some(binding_hash(binding)),
        epoch: Some(epoch),
        generation: Some(generation),
        intent: "published".to_owned(),
        reason: "published".to_owned(),
        event_kind: Some(AuditEvent::ChildStart),
        parent_hash: Some(parent_hash.to_owned()),
        child_hash: Some(child_hash.to_owned()),
        rust: None,
        linkage: None,
    };
    append_record(installation, &record, limits)
}

/// Record an observed child start that has no enforced publication contract.
pub(crate) fn append_unsupported_start(
    installation: &Installation,
    identity: &ChildIdentity,
    limits: GateConfig,
) -> Result<()> {
    let record = Record {
        version: 2,
        event_id: Uuid::new_v4(),
        attempt: Uuid::new_v4(),
        adapter: installation.client(),
        category: ToolCategory::Agent,
        session_hash: None,
        binding_hash: None,
        epoch: None,
        generation: None,
        intent: "deny".to_owned(),
        reason: "unsupported_capability".to_owned(),
        event_kind: Some(AuditEvent::ChildStart),
        parent_hash: Some(session_hash(installation, identity.session_id())?),
        child_hash: Some(identity.store_hash(installation.id(), installation.client())),
        rust: None,
        linkage: None,
    };
    append_record(installation, &record, limits)
}

fn valid_hash(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn valid_rust_record(record: &Record) -> bool {
    let Some(evidence) = &record.rust else {
        return false;
    };
    if evidence.validate().is_err()
        || record.binding_hash.is_none()
        || record.generation.is_none()
        || record.epoch.is_none_or(|epoch| epoch == 0)
        || record.intent == "published"
        || record.reason == "published"
        || !record.linkage.as_deref().is_some_and(valid_hash)
        || linkage(record).ok().as_deref() != record.linkage.as_deref()
    {
        return false;
    }
    let lineage = match record.event_kind {
        None => {
            record.session_hash.is_some()
                && record.parent_hash.is_none()
                && record.child_hash.is_none()
        }
        Some(AuditEvent::ChildPreTool) => {
            record.session_hash.is_none()
                && record.parent_hash.is_some()
                && record.child_hash.is_some()
        }
        Some(AuditEvent::ChildStart | AuditEvent::ParentSpawn) => false,
    };
    let decision = match (
        record.intent.as_str(),
        record.reason.as_str(),
        evidence.analysis,
    ) {
        ("neutral", "checked", RustOutcome::AuditedReadOnly) => evidence.storage.is_none(),
        ("neutral", "checked", RustOutcome::CompleteBuild) => evidence
            .storage
            .as_ref()
            .is_some_and(|p| p.outcome() == Outcome::Pass),
        ("deny", "rust_storage_denied", RustOutcome::CompleteBuild) => evidence
            .storage
            .as_ref()
            .is_some_and(|p| p.outcome() == Outcome::Deny),
        ("deny", "rust_storage_indeterminate", RustOutcome::CompleteBuild) => evidence
            .storage
            .as_ref()
            .is_some_and(|p| p.outcome() == Outcome::Indeterminate),
        (
            "deny",
            "rust_analysis_indeterminate" | "rust_execution_unsupported",
            RustOutcome::Indeterminate,
        ) => true,
        _ => false,
    };
    lineage && decision
}

fn validate_record(record: &Record, installation: &Installation) -> Result<()> {
    if record.event_id.is_nil()
        || record.attempt.is_nil()
        || record.adapter != installation.client()
        || !matches!(record.intent.as_str(), "neutral" | "deny" | "published")
        || !valid_reason(&record.reason)
        || record
            .session_hash
            .as_deref()
            .is_some_and(|hash| !valid_hash(hash))
        || record
            .binding_hash
            .as_deref()
            .is_some_and(|hash| !valid_hash(hash))
        || record.generation.is_some_and(|id| id.is_nil())
        || record
            .parent_hash
            .as_deref()
            .is_some_and(|hash| !valid_hash(hash))
        || record
            .child_hash
            .as_deref()
            .is_some_and(|hash| !valid_hash(hash))
        || match record.version {
            1 => {
                record.rust.is_some()
                    || record.linkage.is_some()
                    || record.reason.starts_with("rust_")
                    || record.event_kind.is_some()
                    || record.parent_hash.is_some()
                    || record.child_hash.is_some()
                    || record.intent == "published"
                    || record.reason == "published"
            }
            2 => {
                record.rust.is_some()
                    || record.linkage.is_some()
                    || record.reason.starts_with("rust_")
                    || record.parent_hash.is_none()
                    || record.session_hash.is_some()
                    || match record.event_kind {
                        Some(AuditEvent::ChildStart) => {
                            record.child_hash.is_none()
                                || record.category != ToolCategory::Agent
                                || (record.intent == "published") != (record.reason == "published")
                        }
                        Some(AuditEvent::ChildPreTool) => {
                            record.child_hash.is_none() || record.intent == "published"
                        }
                        Some(AuditEvent::ParentSpawn) => {
                            record.child_hash.is_some()
                                || record.category != ToolCategory::Agent
                                || record.intent == "published"
                        }
                        None => true,
                    }
            }
            3 => !valid_rust_record(record),
            _ => true,
        }
    {
        return Err(Error::StateCorrupt);
    }
    Ok(())
}

fn append_record(installation: &Installation, record: &Record, limits: GateConfig) -> Result<()> {
    validate_record(record, installation)?;
    let directory = installation.directory();
    let _lock = directory.lock(LOCK)?;
    let current = directory.read(COUNTER)?;
    let counter = current.map_or_else(
        || {
            Ok(Counter {
                version: 1,
                installation_id: installation.id(),
                records: 0,
                bytes: 0,
            })
        },
        |bytes| serde_json::from_slice(&bytes).map_err(|_| Error::StateCorrupt),
    )?;
    if counter.version != 1
        || counter.installation_id != installation.id()
        || counter.records > limits.audit_max_records()
        || counter.bytes > limits.audit_max_bytes()
    {
        return Err(Error::StateCorrupt);
    }
    let mut measured = 0_u32;
    for index in 1..=counter.records {
        let previous = directory.read(&name(index))?.ok_or(Error::StateCorrupt)?;
        if previous.len() > limits.audit_record_bytes() as usize {
            return Err(Error::StateCorrupt);
        }
        let decoded: Record = serde_json::from_slice(&previous).map_err(|_| Error::StateCorrupt)?;
        validate_record(&decoded, installation)?;
        measured = measured
            .checked_add(u32::try_from(previous.len()).map_err(|_| Error::StateCorrupt)?)
            .ok_or(Error::StateCorrupt)?;
    }
    if measured != counter.bytes {
        return Err(Error::StateCorrupt);
    }
    let next = counter.records.checked_add(1).ok_or(Error::StateCorrupt)?;
    if next > limits.audit_max_records() || directory.read(&name(next))?.is_some() {
        return Err(Error::StateCorrupt);
    }
    let bytes = serde_json::to_vec(record).map_err(|_| Error::StateCorrupt)?;
    if bytes.len() > limits.audit_record_bytes() as usize {
        return Err(Error::StateCorrupt);
    }
    let length = u32::try_from(bytes.len()).map_err(|_| Error::StateCorrupt)?;
    let total = counter
        .bytes
        .checked_add(length)
        .ok_or(Error::StateCorrupt)?;
    if total > limits.audit_max_bytes() {
        return Err(Error::StateCorrupt);
    }
    directory.replace(&name(next), &bytes)?;
    if directory.read(&name(next))?.as_deref() != Some(bytes.as_slice()) {
        return Err(Error::StateCorrupt);
    }
    let updated = Counter {
        records: next,
        bytes: total,
        ..counter
    };
    let counter_bytes = serde_json::to_vec(&updated).map_err(|_| Error::StateCorrupt)?;
    directory.replace(COUNTER, &counter_bytes)?;
    if directory.read(COUNTER)?.as_deref() != Some(counter_bytes.as_slice()) {
        return Err(Error::StateCorrupt);
    }
    Ok(())
}

/// Append required safe evidence under the installation anchor lock.
///
/// # Errors
/// Exhaustion, orphan records, corrupt accounting and durability errors deny.
pub(crate) fn append(
    installation: &Installation,
    claim: &ClaimedCheck,
    category: ToolCategory,
    spawn: bool,
    intent: &'static str,
    reason: &'static str,
    limits: GateConfig,
) -> Result<()> {
    append_decision(
        installation,
        claim,
        category,
        spawn,
        (intent, reason),
        limits,
        None,
    )
}

/// Append version three only for a validated Rust analysis handoff.
pub(crate) fn append_rust(
    installation: &Installation,
    claim: &ClaimedCheck,
    category: ToolCategory,
    intent: &'static str,
    reason: &'static str,
    limits: GateConfig,
    evidence: Option<RustEvidence>,
) -> Result<()> {
    append_decision(
        installation,
        claim,
        category,
        false,
        (intent, reason),
        limits,
        evidence,
    )
}

fn append_decision(
    installation: &Installation,
    claim: &ClaimedCheck,
    category: ToolCategory,
    spawn: bool,
    decision: (&'static str, &'static str),
    limits: GateConfig,
    evidence: Option<RustEvidence>,
) -> Result<()> {
    let (intent, reason) = decision;
    let (version, event_kind, parent_hash, child_hash, session_hash) =
        if let Some(parent) = &claim.parent {
            (
                2,
                Some(AuditEvent::ChildPreTool),
                Some(parent.hash().to_owned()),
                Some(claim.hash.clone()),
                None,
            )
        } else if spawn {
            (
                2,
                Some(AuditEvent::ParentSpawn),
                Some(claim.hash.clone()),
                None,
                None,
            )
        } else {
            (1, None, None, None, Some(claim.hash.clone()))
        };
    let mut record = Record {
        version: if evidence.is_some() { 3 } else { version },
        event_id: Uuid::new_v4(),
        attempt: claim.attempt,
        adapter: installation.client(),
        category,
        session_hash,
        binding_hash: Some(binding_hash(&claim.binding)),
        epoch: Some(claim.epoch),
        generation: Some(claim.generation),
        intent: intent.to_owned(),
        reason: reason.to_owned(),
        event_kind,
        parent_hash,
        child_hash,
        rust: evidence,
        linkage: None,
    };
    if record.rust.is_some() {
        record.linkage = Some(linkage(&record)?);
    }
    append_record(installation, &record, limits)
}

/// Audit a denial before a complete snapshot attempt could be claimed.
///
/// # Errors
/// Private-store failures leave the guarded event denied.
pub(crate) fn append_preclaim(
    installation: &Installation,
    event: Option<&PreToolEvent>,
    reason: &'static str,
    limits: GateConfig,
) -> Result<()> {
    let child = event.and_then(PreToolEvent::child);
    let (version, event_kind, parent_hash, child_hash, session_hash) = if let Some(child) = child {
        (
            2,
            Some(AuditEvent::ChildPreTool),
            Some(session_hash(installation, child.session_id())?),
            Some(child.store_hash(installation.id(), installation.client())),
            None,
        )
    } else if event.is_some_and(PreToolEvent::is_agent_operation)
        && installation
            .active()
            .ok()
            .and_then(|active| active.config.delegation())
            .is_some_and(DelegationConfig::enforced)
    {
        (
            2,
            Some(AuditEvent::ParentSpawn),
            event
                .map(|event| session_hash(installation, event.session_id()))
                .transpose()?,
            None,
            None,
        )
    } else {
        (
            1,
            None,
            None,
            None,
            event
                .map(|event| session_hash(installation, event.session_id()))
                .transpose()?,
        )
    };
    let record = Record {
        version,
        event_id: Uuid::new_v4(),
        attempt: Uuid::new_v4(),
        adapter: installation.client(),
        category: event.map_or(ToolCategory::Unknown, PreToolEvent::category),
        session_hash,
        binding_hash: None,
        epoch: None,
        generation: None,
        intent: "deny".to_owned(),
        reason: reason.to_owned(),
        event_kind,
        parent_hash,
        child_hash,
        rust: None,
        linkage: None,
    };
    append_record(installation, &record, limits)
}

#[cfg(test)]
mod tests;
