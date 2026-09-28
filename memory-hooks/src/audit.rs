//! Mandatory anchor-relative, redacted audit evidence for guarded events.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::config::{ClientAdapter, GateConfig, hash_part};
use crate::error::{Error, Result};
use crate::installation::Installation;
use crate::pre_tool::{PreToolEvent, ToolCategory};
use crate::snapshot::{ClaimedCheck, session_hash};

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
            | "policy_changed"
            | "guardrails_unavailable"
            | "snapshot_invalid"
            | "scope_mismatch"
            | "audit_failed"
            | "fresh_session_required"
            | "event_invalid"
            | "gate_contract_required"
            | "unsupported_capability"
    )
}

fn valid_hash(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn validate_record(record: &Record, installation: &Installation) -> Result<()> {
    if record.version != 1
        || record.event_id.is_nil()
        || record.attempt.is_nil()
        || record.adapter != installation.client()
        || !matches!(record.intent.as_str(), "neutral" | "deny")
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
    adapter: ClientAdapter,
    category: ToolCategory,
    intent: &'static str,
    reason: &'static str,
    limits: GateConfig,
) -> Result<()> {
    let record = Record {
        version: 1,
        event_id: Uuid::new_v4(),
        attempt: claim.attempt,
        adapter,
        category,
        session_hash: Some(claim.hash.clone()),
        binding_hash: Some(binding_hash(&claim.binding)),
        epoch: Some(claim.epoch),
        generation: Some(claim.generation),
        intent: intent.to_owned(),
        reason: reason.to_owned(),
    };
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
    let record = Record {
        version: 1,
        event_id: Uuid::new_v4(),
        attempt: Uuid::new_v4(),
        adapter: installation.client(),
        category: event.map_or(ToolCategory::Unknown, PreToolEvent::category),
        session_hash: event
            .map(|event| session_hash(installation, event.session_id()))
            .transpose()?,
        binding_hash: None,
        epoch: None,
        generation: None,
        intent: "deny".to_owned(),
        reason: reason.to_owned(),
    };
    append_record(installation, &record, limits)
}
