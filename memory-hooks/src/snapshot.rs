//! Generation-scoped `SessionStart` delivery and validated snapshot reads.

use std::ffi::OsString;
use std::fs::File;
use std::io::Write;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use memory_common::guardrails::{GuardrailPack, MAX_PUBLICATION_BYTES};
use memory_common::policy::ResolutionContext;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::audit;
use crate::config::{ClientAdapter, hash_part};
use crate::error::{Error, Result, io};
use crate::installation::{ActiveInstallation, Installation};
use crate::limits;
use crate::session_identity::ChildIdentity;
use crate::state::{PrivateDirectory, RootMode};

/// Maximum serialized hook output, including worst-case JSON string escaping.
pub const OUTPUT_BYTES: usize = 192 * 1024;

/// The client event whose exact publication envelope was emitted.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum PublicationEvent {
    /// Top-level session delivery.
    SessionStart,
    /// Child-bound lifecycle delivery.
    SubagentStart,
}

impl PublicationEvent {
    const fn name(self) -> &'static str {
        match self {
            Self::SessionStart => "SessionStart",
            Self::SubagentStart => "SubagentStart",
        }
    }
}

// A JSON string can expand each publication byte to at most six ASCII bytes
// (a \u00XX escape). The fixed envelope, quotes and newline need <256 bytes.
// This accepts every shared-valid 24-KiB publication without truncation.
const _: () = assert!(6 * MAX_PUBLICATION_BYTES + 256 <= OUTPUT_BYTES);

/// Local output lifecycle of a single never-reused generation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryState {
    /// Older delivery has been retired before external validation or fetch.
    Pending,
    /// Exact payload is durable, but complete output has not been recorded.
    Prepared,
    /// All output bytes were written and flushed before this head was committed.
    Emitted,
    /// A current generation failed after it was claimed.
    Invalid,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Head {
    version: u32,
    installation_id: Uuid,
    client: ClientAdapter,
    session_hash: String,
    epoch: u64,
    nonce: Uuid,
    generation: Uuid,
    sequence: u64,
    state: DeliveryState,
    binding: Option<String>,
    payload_name: Option<String>,
    payload_sha256: Option<String>,
    output_sha256: Option<String>,
    output_bytes: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    lineage: Option<ChildLineage>,
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ChildLineage {
    version: u32,
    parent_hash: String,
    parent_epoch: u64,
    parent_nonce: Uuid,
    parent_generation: Uuid,
    parent_sequence: u64,
    parent_binding: String,
}

/// Captured direct-parent authority for one child publication or check.
pub(crate) struct ParentProof {
    lineage: ChildLineage,
    pub(crate) pack: GuardrailPack,
    pub(crate) cwd: PathBuf,
    pub(crate) project: String,
    pub(crate) context: ResolutionContext,
    pub(crate) origin: String,
    pub(crate) api_token: Option<String>,
}

impl ParentProof {
    pub(crate) fn hash(&self) -> &str {
        &self.lineage.parent_hash
    }

    pub(crate) const fn generation(&self) -> Uuid {
        self.lineage.parent_generation
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SessionJournal {
    version: u32,
    generation: Uuid,
    sequence: u64,
    resolved: bool,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Payload {
    version: u32,
    installation_id: Uuid,
    client: ClientAdapter,
    session_hash: String,
    epoch: u64,
    nonce: Uuid,
    generation: Uuid,
    sequence: u64,
    binding: String,
    publication_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    publication_event: Option<PublicationEvent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    lineage: Option<ChildLineage>,
    output_sha256: String,
    output_bytes: u32,
    pack: GuardrailPack,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ParentLocation {
    version: u32,
    installation_id: Uuid,
    client: ClientAdapter,
    session_hash: String,
    epoch: u64,
    nonce: Uuid,
    generation: Uuid,
    sequence: u64,
    binding: String,
    path_hex: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AmbiguityMarker {
    version: u32,
    installation_id: Uuid,
    client: ClientAdapter,
    session_hash: String,
    observed_epoch: u64,
}

#[derive(Clone, Copy, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum GuardState {
    Idle,
    Checking,
    Completed,
    Invalid,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct GuardRecord {
    version: u32,
    installation_id: Uuid,
    client: ClientAdapter,
    session_hash: String,
    epoch: u64,
    nonce: Uuid,
    generation: Uuid,
    sequence: u64,
    state: GuardState,
    attempt: Option<Uuid>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct GuardJournal {
    version: u32,
    generation: Uuid,
    sequence: u64,
    attempt: Option<Uuid>,
    resolved: bool,
}

/// One claimed check; only a matching current attempt may complete it.
pub(crate) struct ClaimedCheck {
    pub(crate) hash: String,
    pub(crate) epoch: u64,
    nonce: Uuid,
    sequence: u64,
    pub(crate) generation: Uuid,
    pub(crate) attempt: Uuid,
    pub(crate) pack: GuardrailPack,
    pub(crate) binding: String,
    cwd: std::path::PathBuf,
    pub(crate) project: String,
    pub(crate) context: ResolutionContext,
    pub(crate) origin: String,
    pub(crate) api_token: Option<String>,
    pub(crate) parent: Option<ParentProof>,
}

#[derive(Serialize)]
struct HookOutput<'a> {
    #[serde(rename = "hookSpecificOutput")]
    hook_specific_output: HookSpecificOutput<'a>,
}

#[derive(Serialize)]
struct HookSpecificOutput<'a> {
    #[serde(rename = "hookEventName")]
    hook_event_name: &'static str,
    #[serde(rename = "additionalContext")]
    additional_context: &'a str,
}

/// Captured generation returned when a session begins refreshing.
pub struct PendingGeneration {
    session_hash: String,
    epoch: u64,
    nonce: Uuid,
    generation: Uuid,
    sequence: u64,
}

impl PendingGeneration {
    /// Activation epoch captured before configurable root or binding lookup.
    #[must_use]
    pub const fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Never-reused identity of this refresh attempt.
    #[must_use]
    pub const fn generation(&self) -> Uuid {
        self.generation
    }
}

/// Exact validated payload returned solely from the current anchor head.
pub struct ValidatedSnapshot {
    /// Complete authoritative mandatory pack.
    pub pack: GuardrailPack,
    /// Current installation epoch for downstream revalidation.
    pub epoch: u64,
    /// Current session generation for downstream revalidation.
    pub generation: Uuid,
    /// Canonical public binding, including repository and cwd identity hashes.
    pub binding: String,
}

fn sha256(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

pub(crate) fn session_hash(installation: &Installation, session: &str) -> Result<String> {
    if session.is_empty() || session.len() > limits::EXTERNAL_ID_BYTES {
        return Err(Error::SessionInvalid("session_id length"));
    }
    let mut hash = Sha256::new();
    hash.update(b"memory-hooks-control-session-v1\0");
    hash_part(&mut hash, installation.id().as_bytes());
    hash_part(&mut hash, installation.client().name().as_bytes());
    hash_part(&mut hash, session.as_bytes());
    Ok(format!("{:x}", hash.finalize()))
}

fn head_name(hash: &str) -> String {
    format!("head-{hash}")
}

fn journal_name(hash: &str) -> String {
    format!("journal-{hash}")
}

fn lock_name(hash: &str) -> String {
    format!("lock-session-{hash}")
}

fn guard_name(hash: &str) -> String {
    format!("guard-{hash}")
}

fn guard_journal_name(hash: &str) -> String {
    format!("guard-journal-{hash}")
}

fn location_name(hash: &str) -> String {
    format!("parent-location-{hash}")
}

fn ambiguity_name(hash: &str) -> String {
    format!("ambiguity-{hash}")
}

fn path_hex(path: &Path) -> Result<String> {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let bytes = path.as_os_str().as_bytes();
    if bytes.is_empty()
        || bytes.len() > limits::PATH_BYTES
        || bytes.contains(&0)
        || !path.is_absolute()
    {
        return Err(Error::SnapshotInvalid("parent location path"));
    }
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(char::from(DIGITS[usize::from(byte >> 4)]));
        encoded.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    Ok(encoded)
}

fn nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

fn path_from_hex(encoded: &str) -> Result<PathBuf> {
    let bytes = encoded.as_bytes();
    if bytes.is_empty() || bytes.len() > 2 * limits::PATH_BYTES || !bytes.len().is_multiple_of(2) {
        return Err(Error::SnapshotInvalid("parent location encoding"));
    }
    let mut decoded = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks_exact(2) {
        let high = nibble(pair[0]).ok_or(Error::SnapshotInvalid("parent location encoding"))?;
        let low = nibble(pair[1]).ok_or(Error::SnapshotInvalid("parent location encoding"))?;
        decoded.push((high << 4) | low);
    }
    if decoded.contains(&0) {
        return Err(Error::SnapshotInvalid("parent location path"));
    }
    let path = PathBuf::from(OsString::from_vec(decoded));
    if !path.is_absolute() {
        return Err(Error::SnapshotInvalid("parent location path"));
    }
    Ok(path)
}

fn read_parent_location(
    installation: &Installation,
    active: &ActiveInstallation,
    head: &Head,
) -> Result<PathBuf> {
    let bytes = installation
        .directory()
        .read(&location_name(&head.session_hash))?
        .ok_or(Error::SnapshotInvalid("parent location missing"))?;
    let location: ParentLocation = serde_json::from_slice(&bytes)
        .map_err(|_| Error::SnapshotInvalid("parent location JSON"))?;
    if location.version != 1
        || location.installation_id != installation.id()
        || location.client != installation.client()
        || location.session_hash != head.session_hash
        || location.epoch != head.epoch
        || location.nonce != head.nonce
        || location.generation != head.generation
        || location.sequence != head.sequence
        || head.binding.as_deref() != Some(location.binding.as_str())
    {
        return Err(Error::SnapshotInvalid("parent location identity"));
    }
    let path = path_from_hex(&location.path_hex)?;
    let canonical = std::fs::canonicalize(&path)
        .map_err(|_| Error::SnapshotInvalid("parent location unavailable"))?;
    if canonical != path || active.config.resolve(&canonical)?.public_json()? != location.binding {
        return Err(Error::SnapshotInvalid("parent location scope"));
    }
    Ok(path)
}

fn replace_parent_location(
    installation: &Installation,
    active: &ActiveInstallation,
    head: &Head,
    cwd: &Path,
) -> Result<()> {
    let actual = std::env::current_dir()
        .and_then(std::fs::canonicalize)
        .map_err(|_| Error::SnapshotInvalid("process cwd"))?;
    if actual != cwd {
        return Err(Error::SnapshotInvalid("parent location cwd mismatch"));
    }
    let location = ParentLocation {
        version: 1,
        installation_id: installation.id(),
        client: installation.client(),
        session_hash: head.session_hash.clone(),
        epoch: head.epoch,
        nonce: head.nonce,
        generation: head.generation,
        sequence: head.sequence,
        binding: head
            .binding
            .clone()
            .ok_or(Error::SnapshotInvalid("parent binding missing"))?,
        path_hex: path_hex(cwd)?,
    };
    let bytes = serde_json::to_vec(&location)
        .map_err(|_| Error::SnapshotInvalid("parent location JSON"))?;
    installation
        .directory()
        .replace(&location_name(&head.session_hash), &bytes)?;
    let readback = read_parent_location(installation, active, head)?;
    if readback != cwd {
        return Err(Error::SnapshotInvalid("parent location readback"));
    }
    Ok(())
}

fn validate_guard(installation: &Installation, head: &Head, guard: &GuardRecord) -> Result<()> {
    if guard.version != 1
        || guard.installation_id != installation.id()
        || guard.client != installation.client()
        || guard.session_hash != head.session_hash
        || guard.epoch != head.epoch
        || guard.nonce != head.nonce
        || guard.generation != head.generation
        || guard.sequence != head.sequence
        || matches!(guard.state, GuardState::Checking | GuardState::Completed)
            != guard.attempt.is_some()
        || guard.attempt.is_some_and(|attempt| attempt.is_nil())
    {
        return Err(Error::SnapshotInvalid("guard identity"));
    }
    Ok(())
}

fn read_guard(installation: &Installation, head: &Head) -> Result<GuardRecord> {
    let directory = installation.directory();
    let bytes = directory
        .read(&guard_name(&head.session_hash))?
        .ok_or(Error::SnapshotInvalid("guard missing"))?;
    let journal = directory
        .read(&guard_journal_name(&head.session_hash))?
        .ok_or(Error::SnapshotInvalid("guard journal missing"))?;
    let guard: GuardRecord =
        serde_json::from_slice(&bytes).map_err(|_| Error::SnapshotInvalid("guard JSON"))?;
    let journal: GuardJournal = serde_json::from_slice(&journal)
        .map_err(|_| Error::SnapshotInvalid("guard journal JSON"))?;
    validate_guard(installation, head, &guard)?;
    if journal.version != 1
        || journal.generation != guard.generation
        || journal.sequence != guard.sequence
        || journal.attempt != guard.attempt
        || !journal.resolved
    {
        return Err(Error::SnapshotInvalid("unresolved guard transition"));
    }
    Ok(guard)
}

fn replace_guard(installation: &Installation, head: &Head, guard: &GuardRecord) -> Result<()> {
    validate_guard(installation, head, guard)?;
    let directory = installation.directory();
    let journal = GuardJournal {
        version: 1,
        generation: guard.generation,
        sequence: guard.sequence,
        attempt: guard.attempt,
        resolved: false,
    };
    let unresolved =
        serde_json::to_vec(&journal).map_err(|_| Error::SnapshotInvalid("guard journal JSON"))?;
    directory.replace(&guard_journal_name(&head.session_hash), &unresolved)?;
    let bytes = serde_json::to_vec(guard).map_err(|_| Error::SnapshotInvalid("guard JSON"))?;
    directory.replace(&guard_name(&head.session_hash), &bytes)?;
    let resolved = serde_json::to_vec(&GuardJournal {
        resolved: true,
        ..journal
    })
    .map_err(|_| Error::SnapshotInvalid("guard journal JSON"))?;
    directory.resolve_journal(
        &guard_journal_name(&head.session_hash),
        &resolved,
        &unresolved,
    )
}

fn payload_name(hash: &str, generation: Uuid) -> String {
    let mut digest = Sha256::new();
    digest.update(b"memory-hooks-payload-name-v1\0");
    hash_part(&mut digest, hash.as_bytes());
    hash_part(&mut digest, generation.as_bytes());
    format!("payload-{:x}", digest.finalize())
}

fn validate_head(head: &Head, installation: &Installation, hash: &str) -> Result<()> {
    if !matches!(head.version, 1 | 2)
        || head.installation_id != installation.id()
        || head.client != installation.client()
        || head.session_hash != hash
        || head.nonce.is_nil()
        || head.generation.is_nil()
        || head.sequence == 0
        || (head.version == 1 && head.lineage.is_some())
        || (head.version == 2
            && matches!(head.state, DeliveryState::Prepared | DeliveryState::Emitted)
            && head.lineage.is_none())
        || head.lineage.as_ref().is_some_and(|lineage| {
            lineage.version != 1
                || lineage.parent_hash.len() != 64
                || !lineage
                    .parent_hash
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())
                || lineage.parent_hash == hash
                || lineage.parent_nonce.is_nil()
                || lineage.parent_generation.is_nil()
                || lineage.parent_sequence == 0
                || lineage.parent_binding.is_empty()
        })
    {
        return Err(Error::SnapshotInvalid("head identity"));
    }
    let complete = head.binding.is_some()
        && head.payload_name.is_some()
        && head.payload_sha256.is_some()
        && head.output_sha256.is_some()
        && head.output_bytes.is_some();
    let has_payload = head.payload_name.is_some()
        || head.payload_sha256.is_some()
        || head.output_sha256.is_some()
        || head.output_bytes.is_some();
    if (matches!(head.state, DeliveryState::Prepared | DeliveryState::Emitted) && !complete)
        || (matches!(head.state, DeliveryState::Pending | DeliveryState::Invalid) && has_payload)
    {
        return Err(Error::SnapshotInvalid("head state"));
    }
    Ok(())
}

fn read_head(installation: &Installation, hash: &str) -> Result<Option<Head>> {
    let directory = installation.directory();
    let bytes = directory.read(&head_name(hash))?;
    let journal = directory.read(&journal_name(hash))?;
    let (bytes, journal) = match (bytes, journal) {
        (Some(bytes), Some(journal)) => (bytes, journal),
        (None, None) => return Ok(None),
        _ => return Err(Error::SnapshotInvalid("head journal missing")),
    };
    let head: Head =
        serde_json::from_slice(&bytes).map_err(|_| Error::SnapshotInvalid("head JSON"))?;
    let journal: SessionJournal =
        serde_json::from_slice(&journal).map_err(|_| Error::SnapshotInvalid("journal JSON"))?;
    validate_head(&head, installation, hash)?;
    if journal.version != 1
        || journal.generation != head.generation
        || journal.sequence != head.sequence
        || !journal.resolved
    {
        return Err(Error::SnapshotInvalid("unresolved head transition"));
    }
    Ok(Some(head))
}

fn replace_head(installation: &Installation, hash: &str, head: &Head) -> Result<()> {
    validate_head(head, installation, hash)?;
    let directory = installation.directory();
    let journal = SessionJournal {
        version: 1,
        generation: head.generation,
        sequence: head.sequence,
        resolved: false,
    };
    let bytes = serde_json::to_vec(&journal).map_err(|_| Error::SnapshotInvalid("journal JSON"))?;
    directory.replace(&journal_name(hash), &bytes)?;
    let bytes = serde_json::to_vec(head).map_err(|_| Error::SnapshotInvalid("head JSON"))?;
    directory.replace(&head_name(hash), &bytes)?;
    let bytes = serde_json::to_vec(&SessionJournal {
        resolved: true,
        ..journal
    })
    .map_err(|_| Error::SnapshotInvalid("journal JSON"))?;
    let unresolved =
        serde_json::to_vec(&journal).map_err(|_| Error::SnapshotInvalid("journal JSON"))?;
    directory.resolve_journal(&journal_name(hash), &bytes, &unresolved)
}

fn current_pending(installation: &Installation, pending: &PendingGeneration) -> Result<Head> {
    let head = read_head(installation, &pending.session_hash)?.ok_or(Error::SessionStale)?;
    if head.epoch != pending.epoch
        || head.nonce != pending.nonce
        || head.generation != pending.generation
        || head.sequence != pending.sequence
        || head.state != DeliveryState::Pending
    {
        return Err(Error::SessionStale);
    }
    Ok(head)
}

/// Serialize an exact, bounded hook-specific publication for a lifecycle event.
///
/// # Errors
/// Rejects a pack whose publication or JSON envelope exceeds shared limits.
pub fn serialize_publication(pack: &GuardrailPack, event: PublicationEvent) -> Result<Vec<u8>> {
    let publication = pack
        .publication()
        .map_err(|_| Error::SnapshotInvalid("publication"))?;
    let output = HookOutput {
        hook_specific_output: HookSpecificOutput {
            hook_event_name: event.name(),
            additional_context: &publication,
        },
    };
    let mut bytes =
        serde_json::to_vec(&output).map_err(|_| Error::SnapshotInvalid("output JSON"))?;
    bytes.push(b'\n');
    if bytes.len() > OUTPUT_BYTES {
        return Err(Error::OutputTooLarge);
    }
    Ok(bytes)
}

fn exact_output(pack: &GuardrailPack) -> Result<Vec<u8>> {
    serialize_publication(pack, PublicationEvent::SessionStart)
}

fn child_payload(
    installation: &Installation,
    pending: &PendingGeneration,
    proof: &ParentProof,
    binding: String,
    output_sha256: String,
    output_bytes: u32,
) -> Payload {
    Payload {
        version: 2,
        installation_id: installation.id(),
        client: installation.client(),
        session_hash: pending.session_hash.clone(),
        epoch: pending.epoch,
        nonce: pending.nonce,
        generation: pending.generation,
        sequence: pending.sequence,
        binding,
        publication_version: 2,
        publication_event: Some(PublicationEvent::SubagentStart),
        lineage: Some(proof.lineage.clone()),
        output_sha256,
        output_bytes,
        pack: proof.pack.clone(),
    }
}

fn validate_payload(
    installation: &Installation,
    head: &Head,
    root: &PrivateDirectory,
    binding: &str,
    project: &str,
    context: &memory_common::policy::ResolutionContext,
) -> Result<GuardrailPack> {
    let name = head
        .payload_name
        .as_deref()
        .ok_or(Error::SnapshotInvalid("payload reference"))?;
    if name != payload_name(&head.session_hash, head.generation) {
        return Err(Error::SnapshotInvalid("payload name"));
    }
    let bytes = root
        .read(name)?
        .ok_or(Error::SnapshotInvalid("payload missing"))?;
    if head.payload_sha256.as_deref() != Some(sha256(&bytes).as_str()) {
        return Err(Error::SnapshotInvalid("payload hash"));
    }
    let payload: Payload =
        serde_json::from_slice(&bytes).map_err(|_| Error::SnapshotInvalid("payload JSON"))?;
    if payload.version != head.version
        || payload.installation_id != installation.id()
        || payload.client != installation.client()
        || payload.session_hash != head.session_hash
        || payload.epoch != head.epoch
        || payload.nonce != head.nonce
        || payload.generation != head.generation
        || payload.sequence != head.sequence
        || payload.binding != binding
        || head.binding.as_deref() != Some(binding)
        || payload.lineage != head.lineage
        || match head.version {
            1 => payload.publication_version != 1 || payload.publication_event.is_some(),
            2 => {
                payload.publication_version != 2
                    || payload.publication_event != Some(PublicationEvent::SubagentStart)
            }
            _ => true,
        }
        || head.output_sha256.as_deref() != Some(payload.output_sha256.as_str())
        || head.output_bytes != Some(payload.output_bytes)
    {
        return Err(Error::SnapshotInvalid("payload scope"));
    }
    payload
        .pack
        .validate_for(project, context)
        .map_err(|_| Error::SnapshotInvalid("pack validation"))?;
    let output = serialize_publication(
        &payload.pack,
        payload
            .publication_event
            .unwrap_or(PublicationEvent::SessionStart),
    )?;
    if sha256(&output) != payload.output_sha256
        || u32::try_from(output.len()).ok() != Some(payload.output_bytes)
    {
        return Err(Error::SnapshotInvalid("output evidence"));
    }
    Ok(payload.pack)
}

fn retire_current_locked(installation: &Installation, hash: &str) -> Result<()> {
    match read_head(installation, hash) {
        Ok(Some(mut head)) if head.state == DeliveryState::Emitted => {
            head.state = DeliveryState::Invalid;
            head.payload_name = None;
            head.payload_sha256 = None;
            head.output_sha256 = None;
            head.output_bytes = None;
            replace_head(installation, hash, &head)
        }
        Ok(_) => Ok(()),
        Err(_) => installation.poison_locked(),
    }
}

fn invalidate_concurrent_locked(
    installation: &Installation,
    hash: &str,
    head: &Head,
    mut guard: GuardRecord,
) -> Result<()> {
    guard.state = GuardState::Invalid;
    guard.attempt = None;
    replace_guard(installation, head, &guard)?;
    let mut invalid = head.clone();
    invalid.state = DeliveryState::Invalid;
    invalid.payload_name = None;
    invalid.payload_sha256 = None;
    invalid.output_sha256 = None;
    invalid.output_bytes = None;
    replace_head(installation, hash, &invalid)
}

fn missing_check_head(child: bool) -> Error {
    Error::SnapshotInvalid(if child {
        "child head missing"
    } else {
        "head missing"
    })
}

impl Installation {
    /// Persist an unsupported child lifecycle for the enclosing session.
    ///
    /// Ordinary root refresh cannot remove this marker because subsequent
    /// same-session callbacks may be indistinguishable from that child.
    pub(crate) fn mark_ambiguous(&self, session: &str) -> Result<()> {
        let hash = session_hash(self, session)?;
        let _installation_lock = self.lock()?;
        let active = self.active_locked()?;
        let _session_lock = self.directory().lock(&lock_name(&hash))?;
        let marker = AmbiguityMarker {
            version: 1,
            installation_id: self.id(),
            client: self.client(),
            session_hash: hash.clone(),
            observed_epoch: active.epoch,
        };
        let bytes =
            serde_json::to_vec(&marker).map_err(|_| Error::SnapshotInvalid("ambiguity marker"))?;
        let durable = (|| {
            self.directory().replace(&ambiguity_name(&hash), &bytes)?;
            if self.directory().read(&ambiguity_name(&hash))?.as_deref() != Some(bytes.as_slice()) {
                return Err(Error::SnapshotInvalid("ambiguity readback"));
            }
            Ok(())
        })();
        if durable.is_err() {
            self.poison_locked()?;
        }
        durable
    }

    /// Whether unsupported observed delegation makes a root-shaped call ambiguous.
    pub(crate) fn is_ambiguous(&self, session: &str) -> Result<bool> {
        let hash = session_hash(self, session)?;
        let _installation_lock = self.lock()?;
        let _active = self.active_locked()?;
        let _session_lock = self.directory().lock(&lock_name(&hash))?;
        let Some(bytes) = self.directory().read(&ambiguity_name(&hash))? else {
            return Ok(false);
        };
        let marker: AmbiguityMarker = serde_json::from_slice(&bytes)
            .map_err(|_| Error::SnapshotInvalid("ambiguity marker"))?;
        if marker.version != 1
            || marker.installation_id != self.id()
            || marker.client != self.client()
            || marker.session_hash != hash
            || marker.observed_epoch == 0
        {
            return Err(Error::SnapshotInvalid("ambiguity identity"));
        }
        Ok(true)
    }
    fn lock_pair(&self, left: &str, right: &str) -> Result<(File, File)> {
        if left == right {
            return Err(Error::SnapshotInvalid("identity key collision"));
        }
        let (first, second) = if left < right {
            (left, right)
        } else {
            (right, left)
        };
        let first_lock = self.directory().lock(&lock_name(first))?;
        let second_lock = self.directory().lock(&lock_name(second))?;
        Ok((first_lock, second_lock))
    }
    /// Retire an unambiguously identified session after a malformed event.
    ///
    /// # Errors
    /// Corrupt lineage poisons valid installation control authority.
    pub(crate) fn reject_event_session(&self, session: &str) -> Result<()> {
        let hash = session_hash(self, session)?;
        let _installation_lock = self.lock()?;
        let _active = self.active_locked()?;
        let _session_lock = self.directory().lock(&lock_name(&hash))?;
        retire_current_locked(self, &hash)
    }

    /// Retire only the unambiguously identified child after malformed input.
    pub(crate) fn reject_event_child(&self, identity: &ChildIdentity) -> Result<()> {
        let hash = identity.store_hash(self.id(), self.client());
        let _installation_lock = self.lock()?;
        let _active = self.active_locked()?;
        let _child_lock = self.directory().lock(&lock_name(&hash))?;
        retire_current_locked(self, &hash)
    }

    /// Retire only the generation observed before an outer worker failure.
    ///
    /// # Errors
    /// A corrupt current head poisons installation authority.
    pub(crate) fn reject_observed_generation(&self, session: &str, generation: Uuid) -> Result<()> {
        let hash = session_hash(self, session)?;
        self.reject_hash_generation(&hash, generation)
    }

    pub(crate) fn reject_observed_child_generation(
        &self,
        identity: &ChildIdentity,
        generation: Uuid,
    ) -> Result<()> {
        let hash = identity.store_hash(self.id(), self.client());
        self.reject_hash_generation(&hash, generation)
    }

    fn reject_hash_generation(&self, hash: &str, generation: Uuid) -> Result<()> {
        let _installation_lock = self.lock()?;
        let _session_lock = self.directory().lock(&lock_name(hash))?;
        match read_head(self, hash) {
            Ok(Some(head)) if head.generation == generation => retire_current_locked(self, hash),
            Ok(_) => Ok(()),
            Err(error) => {
                self.poison_locked()?;
                Err(error)
            }
        }
    }

    /// Advance an unresolved session transition to a fresh pending tombstone.
    ///
    /// This is an explicit administrative repair. It uses the largest checked
    /// sequence visible in the head or journal and never reinstates Emitted.
    /// A missing or corrupt journal cannot be repaired by guessing lineage.
    ///
    /// # Errors
    /// Refuses a clean, corrupt or inactive installation/session transition.
    pub fn repair_session(&self, session: &str) -> Result<u64> {
        let hash = session_hash(self, session)?;
        let _installation_lock = self.lock()?;
        let (epoch, nonce) = self.epoch_locked()?;
        let _session_lock = self.directory().lock(&lock_name(&hash))?;
        let journal_bytes = self
            .directory()
            .read(&journal_name(&hash))?
            .ok_or(Error::SnapshotInvalid("repair journal missing"))?;
        let journal: SessionJournal = serde_json::from_slice(&journal_bytes)
            .map_err(|_| Error::SnapshotInvalid("repair journal JSON"))?;
        if journal.version != 1
            || journal.resolved
            || journal.generation.is_nil()
            || journal.sequence == 0
        {
            return Err(Error::SnapshotInvalid("repair journal state"));
        }
        let head = self
            .directory()
            .read(&head_name(&hash))?
            .map(|bytes| {
                let head: Head = serde_json::from_slice(&bytes)
                    .map_err(|_| Error::SnapshotInvalid("repair head JSON"))?;
                validate_head(&head, self, &hash)?;
                Ok::<_, Error>(head)
            })
            .transpose()?;
        if head.as_ref().is_some_and(|head| {
            journal.sequence < head.sequence
                || journal.sequence > head.sequence.saturating_add(1)
                || (journal.sequence == head.sequence && journal.generation != head.generation)
        }) {
            return Err(Error::SnapshotInvalid("repair lineage"));
        }
        let sequence = journal
            .sequence
            .checked_add(1)
            .ok_or(Error::SessionInvalid("sequence overflow"))?;
        let repaired = Head {
            version: 1,
            installation_id: self.id(),
            client: self.client(),
            session_hash: hash.clone(),
            epoch,
            nonce,
            generation: Uuid::new_v4(),
            sequence,
            state: DeliveryState::Pending,
            binding: None,
            payload_name: None,
            payload_sha256: None,
            output_sha256: None,
            output_bytes: None,
            lineage: None,
        };
        replace_head(self, &hash, &repaired)?;
        Ok(sequence)
    }

    /// Claim a fresh generation before any configurable root or binding lookup.
    ///
    /// # Errors
    /// Missing, corrupt or unresolved authority never falls back to old heads.
    pub fn begin_session(&self, session: &str) -> Result<PendingGeneration> {
        let hash = session_hash(self, session)?;
        let _installation_lock = self.lock()?;
        let (epoch, nonce) = self.epoch_locked()?;
        let _session_lock = self.directory().lock(&lock_name(&hash))?;
        let sequence = read_head(self, &hash)?.map_or(Ok(1), |head| {
            head.sequence
                .checked_add(1)
                .ok_or(Error::SessionInvalid("sequence overflow"))
        })?;
        let generation = Uuid::new_v4();
        let head = Head {
            version: 1,
            installation_id: self.id(),
            client: self.client(),
            session_hash: hash.clone(),
            epoch,
            nonce,
            generation,
            sequence,
            state: DeliveryState::Pending,
            binding: None,
            payload_name: None,
            payload_sha256: None,
            output_sha256: None,
            output_bytes: None,
            lineage: None,
        };
        replace_head(self, &hash, &head)?;
        Ok(PendingGeneration {
            session_hash: hash,
            epoch,
            nonce,
            generation,
            sequence,
        })
    }

    /// Claim a distinct child generation before fetching any delegated scope.
    ///
    /// A repeated child ID is tombstoned. The pinned lifecycle does not prove
    /// replacement of previously injected context, so it cannot refresh an
    /// existing child's authority under the same identifier.
    pub(crate) fn begin_child(&self, identity: &ChildIdentity) -> Result<PendingGeneration> {
        let hash = identity.store_hash(self.id(), self.client());
        let _installation_lock = self.lock()?;
        let (epoch, nonce) = self.epoch_locked()?;
        let _child_lock = self.directory().lock(&lock_name(&hash))?;
        let previous = read_head(self, &hash)?;
        if previous.as_ref().is_some_and(|head| head.version != 2) {
            return Err(Error::SnapshotInvalid("child key domain"));
        }
        let sequence = previous.as_ref().map_or(Ok(1), |head| {
            head.sequence
                .checked_add(1)
                .ok_or(Error::SessionInvalid("sequence overflow"))
        })?;
        let generation = Uuid::new_v4();
        let head = Head {
            version: 2,
            installation_id: self.id(),
            client: self.client(),
            session_hash: hash.clone(),
            epoch,
            nonce,
            generation,
            sequence,
            state: DeliveryState::Pending,
            binding: None,
            payload_name: None,
            payload_sha256: None,
            output_sha256: None,
            output_bytes: None,
            lineage: None,
        };
        replace_head(self, &hash, &head)?;
        if previous.is_some() {
            return Err(Error::SessionInvalid("repeat child start"));
        }
        Ok(PendingGeneration {
            session_hash: hash,
            epoch,
            nonce,
            generation,
            sequence,
        })
    }

    /// Resolve and attach the current protected binding to a pending head.
    ///
    /// # Errors
    /// Rejects a changed epoch, newer generation, or unmapped cwd.
    pub fn attach_binding(
        &self,
        pending: &PendingGeneration,
        cwd: &Path,
    ) -> Result<crate::identity::ResolvedBinding> {
        let _installation_lock = self.lock()?;
        let active = self.active_locked()?;
        if active.epoch != pending.epoch || active.nonce != pending.nonce {
            return Err(Error::SessionStale);
        }
        let _session_lock = self.directory().lock(&lock_name(&pending.session_hash))?;
        let mut head = current_pending(self, pending)?;
        let binding = active.config.resolve(cwd)?;
        head.binding = Some(binding.public_json()?);
        replace_head(self, &pending.session_hash, &head)?;
        Ok(binding)
    }

    /// Persist exact payload, emit all bytes, then commit Emitted last.
    ///
    /// The supplied writer must enforce a finite output deadline. A failed
    /// write or flush leaves Prepared unusable to the public reader.
    ///
    /// # Errors
    /// Rejects stale, invalid or oversized snapshots; propagates writer errors.
    pub fn complete_session<W: Write>(
        &self,
        pending: &PendingGeneration,
        cwd: &Path,
        pack: GuardrailPack,
        writer: &mut W,
    ) -> Result<()> {
        let output = exact_output(&pack)?;
        let output_hash = sha256(&output);
        let output_bytes = u32::try_from(output.len()).map_err(|_| Error::OutputTooLarge)?;
        let _installation_lock = self.lock()?;
        let active = self.active_locked()?;
        if active.epoch != pending.epoch || active.nonce != pending.nonce {
            return Err(Error::SessionStale);
        }
        let _session_lock = self.directory().lock(&lock_name(&pending.session_hash))?;
        let mut head = current_pending(self, pending)?;
        let binding = active.config.resolve(cwd)?;
        let binding_json = binding.public_json()?;
        if head.binding.as_deref() != Some(binding_json.as_str()) {
            return Err(Error::SessionStale);
        }
        pack.validate_for(binding.project(), binding.context())
            .map_err(|_| Error::SnapshotInvalid("pack validation"))?;
        let root = PrivateDirectory::open(&active.config.state_root, RootMode::Existing)?;
        let name = payload_name(&pending.session_hash, pending.generation);
        let payload = Payload {
            version: 1,
            installation_id: self.id(),
            client: self.client(),
            session_hash: pending.session_hash.clone(),
            epoch: pending.epoch,
            nonce: pending.nonce,
            generation: pending.generation,
            sequence: pending.sequence,
            binding: binding_json,
            publication_version: 1,
            publication_event: None,
            lineage: None,
            output_sha256: output_hash.clone(),
            output_bytes,
            pack,
        };
        let bytes =
            serde_json::to_vec(&payload).map_err(|_| Error::SnapshotInvalid("payload JSON"))?;
        root.replace(&name, &bytes)?;
        head.state = DeliveryState::Prepared;
        head.payload_name = Some(name);
        head.payload_sha256 = Some(sha256(&bytes));
        head.output_sha256 = Some(output_hash);
        head.output_bytes = Some(output_bytes);
        replace_head(self, &pending.session_hash, &head)?;
        if active.config.delegation().is_some() {
            replace_parent_location(self, &active, &head, cwd)?;
        }
        writer
            .write_all(&output)
            .map_err(|error| io("output write", &error))?;
        writer.flush().map_err(|error| io("output flush", &error))?;
        if active.config.gate().is_some() {
            replace_guard(
                self,
                &head,
                &GuardRecord {
                    version: 1,
                    installation_id: self.id(),
                    client: self.client(),
                    session_hash: pending.session_hash.clone(),
                    epoch: pending.epoch,
                    nonce: pending.nonce,
                    generation: pending.generation,
                    sequence: pending.sequence,
                    state: GuardState::Idle,
                    attempt: None,
                },
            )?;
        }
        head.state = DeliveryState::Emitted;
        replace_head(self, &pending.session_hash, &head)?;
        let readback = read_head(self, &pending.session_hash)?
            .ok_or(Error::SnapshotInvalid("head missing after emission"))?;
        if readback.generation != pending.generation || readback.state != DeliveryState::Emitted {
            return Err(Error::SnapshotInvalid("emission readback"));
        }
        Ok(())
    }

    /// Publish one child-bound snapshot from a freshly verified direct parent.
    ///
    /// The final parent and child comparison orders against root refresh and
    /// child mutation. A write, flush, audit or durability failure cannot leave
    /// an Emitted child generation.
    pub(crate) fn complete_child<W: Write>(
        &self,
        identity: &ChildIdentity,
        pending: &PendingGeneration,
        proof: &ParentProof,
        writer: &mut W,
    ) -> Result<()> {
        if pending.session_hash != identity.store_hash(self.id(), self.client())
            || proof.lineage.parent_hash != session_hash(self, identity.session_id())?
        {
            return Err(Error::SessionStale);
        }
        let output = serialize_publication(&proof.pack, PublicationEvent::SubagentStart)?;
        let output_hash = sha256(&output);
        let output_bytes = u32::try_from(output.len()).map_err(|_| Error::OutputTooLarge)?;
        let _installation_lock = self.lock()?;
        let active = self.active_locked()?;
        if active.epoch != pending.epoch || active.nonce != pending.nonce {
            return Err(Error::SessionStale);
        }
        let (_first_lock, _second_lock) =
            self.lock_pair(&proof.lineage.parent_hash, &pending.session_hash)?;
        self.validate_parent_locked(&active, proof)?;
        let mut head = current_pending(self, pending)?;
        if head.version != 2 {
            return Err(Error::SnapshotInvalid("child head version"));
        }
        let binding = active.config.resolve(&proof.cwd)?;
        let binding_json = binding.public_json()?;
        if head.binding.as_deref() != Some(binding_json.as_str())
            || binding_json != proof.lineage.parent_binding
        {
            return Err(Error::SnapshotInvalid("child scope"));
        }
        proof
            .pack
            .validate_for(binding.project(), binding.context())
            .map_err(|_| Error::SnapshotInvalid("child pack"))?;
        let root = PrivateDirectory::open(&active.config.state_root, RootMode::Existing)?;
        let name = payload_name(&pending.session_hash, pending.generation);
        let payload = child_payload(
            self,
            pending,
            proof,
            binding_json,
            output_hash.clone(),
            output_bytes,
        );
        let bytes =
            serde_json::to_vec(&payload).map_err(|_| Error::SnapshotInvalid("child payload"))?;
        root.replace(&name, &bytes)?;
        head.state = DeliveryState::Prepared;
        head.lineage = Some(proof.lineage.clone());
        head.payload_name = Some(name);
        head.payload_sha256 = Some(sha256(&bytes));
        head.output_sha256 = Some(output_hash);
        head.output_bytes = Some(output_bytes);
        replace_head(self, &pending.session_hash, &head)?;
        replace_parent_location(self, &active, &head, &proof.cwd)?;
        writer
            .write_all(&output)
            .map_err(|error| io("child output write", &error))?;
        writer
            .flush()
            .map_err(|error| io("child output flush", &error))?;
        let gate = active
            .config
            .gate()
            .ok_or(Error::ConfigInvalid("gate_contract_required"))?;
        audit::append_child_start(
            self,
            &pending.session_hash,
            &proof.lineage.parent_hash,
            head.binding
                .as_deref()
                .ok_or(Error::SnapshotInvalid("child binding"))?,
            head.epoch,
            head.generation,
            gate,
        )
        .map_err(|_| Error::AuditFailed)?;
        replace_guard(
            self,
            &head,
            &GuardRecord {
                version: 1,
                installation_id: self.id(),
                client: self.client(),
                session_hash: pending.session_hash.clone(),
                epoch: pending.epoch,
                nonce: pending.nonce,
                generation: pending.generation,
                sequence: pending.sequence,
                state: GuardState::Idle,
                attempt: None,
            },
        )?;
        head.state = DeliveryState::Emitted;
        replace_head(self, &pending.session_hash, &head)?;
        let readback = read_head(self, &pending.session_hash)?
            .ok_or(Error::SnapshotInvalid("child emission readback"))?;
        if readback.state != DeliveryState::Emitted || readback.generation != pending.generation {
            return Err(Error::SnapshotInvalid("child emission readback"));
        }
        Ok(())
    }

    /// Read the only usable snapshot: the current emitted head and exact payload.
    ///
    /// The caller must revalidate epoch and generation before later actions.
    /// This reader does not authorize mutations.
    ///
    /// # Errors
    /// Pending, prepared, copied, corrupt, stale or missing data is unusable.
    pub fn read_snapshot(&self, session: &str, cwd: &Path) -> Result<ValidatedSnapshot> {
        let hash = session_hash(self, session)?;
        let _installation_lock = self.lock()?;
        let active = self.active_locked()?;
        let _session_lock = self.directory().lock(&lock_name(&hash))?;
        self.read_snapshot_locked(&active, &hash, cwd)
    }

    /// Read a child only while its exact direct parent still validates.
    pub(crate) fn read_child_snapshot(
        &self,
        identity: &ChildIdentity,
        cwd: &Path,
    ) -> Result<ValidatedSnapshot> {
        let parent = self.capture_parent(identity.session_id(), cwd)?;
        let child_hash = identity.store_hash(self.id(), self.client());
        let _installation_lock = self.lock()?;
        let active = self.active_locked()?;
        let (_first_lock, _second_lock) =
            self.lock_pair(&parent.lineage.parent_hash, &child_hash)?;
        self.validate_parent_locked(&active, &parent)?;
        let head =
            read_head(self, &child_hash)?.ok_or(Error::SnapshotInvalid("child head missing"))?;
        if head.version != 2 || head.lineage.as_ref() != Some(&parent.lineage) {
            return Err(Error::SnapshotInvalid("child lineage"));
        }
        let snapshot = self.read_snapshot_locked(&active, &child_hash, cwd)?;
        if snapshot.pack != parent.pack {
            return Err(Error::SnapshotInvalid("child pack changed"));
        }
        Ok(snapshot)
    }

    /// Capture current direct-root authority without claiming its guard.
    ///
    /// Sibling children can validate the same parent concurrently without
    /// invalidating one another. The proof must be compared again under the
    /// installation and both session locks before child output or permission.
    pub(crate) fn capture_parent(&self, session: &str, cwd: &Path) -> Result<ParentProof> {
        let hash = session_hash(self, session)?;
        let _installation_lock = self.lock()?;
        let active = self.active_locked()?;
        let delivery = active
            .config
            .delivery()
            .ok_or(Error::InstallationInactive)?;
        let _parent_lock = self.directory().lock(&lock_name(&hash))?;
        let head = read_head(self, &hash)?.ok_or(Error::SnapshotInvalid("parent missing"))?;
        if head.version != 1 || read_parent_location(self, &active, &head)? != cwd {
            return Err(Error::SnapshotInvalid("parent location"));
        }
        let snapshot = self.read_snapshot_locked(&active, &hash, cwd)?;
        let binding = active.config.resolve(cwd)?;
        Ok(ParentProof {
            lineage: ChildLineage {
                version: 1,
                parent_hash: hash,
                parent_epoch: head.epoch,
                parent_nonce: head.nonce,
                parent_generation: head.generation,
                parent_sequence: head.sequence,
                parent_binding: snapshot.binding,
            },
            pack: snapshot.pack,
            cwd: cwd.to_path_buf(),
            project: binding.project().to_owned(),
            context: binding.context().clone(),
            origin: delivery.origin().to_owned(),
            api_token: delivery.api_token().map(str::to_owned),
        })
    }

    fn validate_parent_locked(
        &self,
        active: &ActiveInstallation,
        proof: &ParentProof,
    ) -> Result<()> {
        let lineage = &proof.lineage;
        let head = read_head(self, &lineage.parent_hash)?
            .ok_or(Error::SnapshotInvalid("parent missing"))?;
        if head.version != 1
            || head.epoch != lineage.parent_epoch
            || head.nonce != lineage.parent_nonce
            || head.generation != lineage.parent_generation
            || head.sequence != lineage.parent_sequence
            || head.binding.as_deref() != Some(lineage.parent_binding.as_str())
            || read_parent_location(self, active, &head)? != proof.cwd
        {
            return Err(Error::SnapshotInvalid("parent changed"));
        }
        let snapshot = self.read_snapshot_locked(active, &lineage.parent_hash, &proof.cwd)?;
        if snapshot.pack != proof.pack || snapshot.binding != lineage.parent_binding {
            return Err(Error::SnapshotInvalid("parent changed"));
        }
        Ok(())
    }

    fn read_snapshot_locked(
        &self,
        active: &ActiveInstallation,
        hash: &str,
        cwd: &Path,
    ) -> Result<ValidatedSnapshot> {
        let head = read_head(self, hash)?.ok_or(Error::SnapshotInvalid("head missing"))?;
        if head.epoch != active.epoch
            || head.nonce != active.nonce
            || head.state != DeliveryState::Emitted
        {
            return Err(Error::SnapshotInvalid("head not current emitted"));
        }
        if active.config.gate().is_some()
            && !matches!(
                read_guard(self, &head)?.state,
                GuardState::Idle | GuardState::Completed
            )
        {
            return Err(Error::SnapshotInvalid("guard unavailable"));
        }
        let binding = active.config.resolve(cwd)?;
        let binding_json = binding.public_json()?;
        let root = PrivateDirectory::open(&active.config.state_root, RootMode::Existing)?;
        let pack = validate_payload(
            self,
            &head,
            &root,
            &binding_json,
            binding.project(),
            binding.context(),
        )?;
        Ok(ValidatedSnapshot {
            pack,
            epoch: head.epoch,
            generation: head.generation,
            binding: binding_json,
        })
    }

    /// Claim a fresh check, making this generation unreadable until it resolves.
    ///
    /// # Errors
    /// A missing delivery, concurrent check, or nondurable marker denies use.
    pub(crate) fn claim_check(&self, session: &str, cwd: &Path) -> Result<ClaimedCheck> {
        let hash = session_hash(self, session)?;
        self.claim_hash_check(hash, cwd, None)
    }

    /// Claim only this child guard while retaining a separately validated root.
    pub(crate) fn claim_child_check(
        &self,
        identity: &ChildIdentity,
        cwd: &Path,
    ) -> Result<ClaimedCheck> {
        let parent = self.capture_parent(identity.session_id(), cwd)?;
        let hash = identity.store_hash(self.id(), self.client());
        self.claim_hash_check(hash, cwd, Some(parent))
    }

    fn claim_hash_check(
        &self,
        hash: String,
        cwd: &Path,
        parent: Option<ParentProof>,
    ) -> Result<ClaimedCheck> {
        let _installation_lock = self.lock()?;
        let active = self.active_locked()?;
        let delivery = active
            .config
            .delivery()
            .ok_or(Error::InstallationInactive)?;
        if active.config.gate().is_none() {
            return Err(Error::ConfigInvalid("gate_contract_required"));
        }
        let _locks = if let Some(proof) = &parent {
            let locks = self.lock_pair(&proof.lineage.parent_hash, &hash)?;
            self.validate_parent_locked(&active, proof)?;
            (locks.0, Some(locks.1))
        } else {
            (self.directory().lock(&lock_name(&hash))?, None)
        };
        let head = match read_head(self, &hash) {
            Ok(Some(head)) => head,
            Ok(None) => return Err(missing_check_head(parent.is_some())),
            Err(error) => {
                self.poison_locked()?;
                return Err(error);
            }
        };
        if head.version != if parent.is_some() { 2 } else { 1 }
            || parent
                .as_ref()
                .is_some_and(|proof| head.lineage.as_ref() != Some(&proof.lineage))
        {
            return Err(Error::SnapshotInvalid("check lineage"));
        }
        let mut guard = match read_guard(self, &head) {
            Ok(guard) => guard,
            Err(error) => {
                retire_current_locked(self, &hash)?;
                return Err(error);
            }
        };
        if guard.state == GuardState::Checking {
            invalidate_concurrent_locked(self, &hash, &head, guard)?;
            return Err(Error::SnapshotInvalid("concurrent check"));
        }
        if !matches!(guard.state, GuardState::Idle | GuardState::Completed) {
            return Err(Error::SnapshotInvalid("guard invalid"));
        }
        let snapshot = match self.read_snapshot_locked(&active, &hash, cwd) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                retire_current_locked(self, &hash)?;
                return Err(error);
            }
        };
        if parent
            .as_ref()
            .is_some_and(|proof| snapshot.pack != proof.pack)
        {
            retire_current_locked(self, &hash)?;
            return Err(Error::SnapshotInvalid("child pack changed"));
        }
        let binding = active.config.resolve(cwd)?;
        guard.state = GuardState::Checking;
        guard.attempt = Some(Uuid::new_v4());
        if let Err(error) = replace_guard(self, &head, &guard) {
            retire_current_locked(self, &hash)?;
            return Err(error);
        }
        let readback = match read_guard(self, &head) {
            Ok(readback) => readback,
            Err(error) => {
                retire_current_locked(self, &hash)?;
                return Err(error);
            }
        };
        if readback.state != GuardState::Checking || readback.attempt != guard.attempt {
            retire_current_locked(self, &hash)?;
            return Err(Error::SnapshotInvalid("guard claim readback"));
        }
        Ok(ClaimedCheck {
            hash,
            epoch: head.epoch,
            nonce: head.nonce,
            sequence: head.sequence,
            generation: head.generation,
            attempt: guard
                .attempt
                .ok_or(Error::SnapshotInvalid("guard attempt"))?,
            pack: snapshot.pack,
            binding: snapshot.binding,
            cwd: cwd.to_path_buf(),
            project: binding.project().to_owned(),
            context: binding.context().clone(),
            origin: delivery.origin().to_owned(),
            api_token: delivery.api_token().map(str::to_owned),
            parent,
        })
    }

    /// Conditionally invalidate only the captured generation and check.
    ///
    /// # Errors
    /// Returns an error when state cannot be durably retired.
    pub(crate) fn fail_check(&self, claim: &ClaimedCheck) -> Result<()> {
        self.fail_hash_attempt(&claim.hash, claim.generation, claim.attempt)
    }

    /// Invalidate a supervisor-observed attempt without trusting worker success.
    ///
    /// # Errors
    /// Returns an error when the matching state cannot be retired durably.
    pub(crate) fn fail_attempt(
        &self,
        session: &str,
        generation: Uuid,
        attempt: Uuid,
    ) -> Result<()> {
        let hash = session_hash(self, session)?;
        self.fail_hash_attempt(&hash, generation, attempt)
    }

    pub(crate) fn fail_child_attempt(
        &self,
        identity: &ChildIdentity,
        generation: Uuid,
        attempt: Uuid,
    ) -> Result<()> {
        let hash = identity.store_hash(self.id(), self.client());
        self.fail_hash_attempt(&hash, generation, attempt)
    }

    fn fail_hash_attempt(&self, hash: &str, generation: Uuid, attempt: Uuid) -> Result<()> {
        let _installation_lock = self.lock()?;
        let _session_lock = self.directory().lock(&lock_name(hash))?;
        let mut head = match read_head(self, hash) {
            Ok(Some(head)) => head,
            Ok(None) => return Err(Error::SnapshotInvalid("head missing")),
            Err(error) => {
                self.poison_locked()?;
                return Err(error);
            }
        };
        if head.generation != generation {
            return Ok(());
        }
        let mut guard = match read_guard(self, &head) {
            Ok(guard) => guard,
            Err(error) => {
                retire_current_locked(self, hash)?;
                return Err(error);
            }
        };
        if !matches!(guard.state, GuardState::Checking | GuardState::Completed)
            || guard.attempt != Some(attempt)
        {
            return Ok(());
        }
        guard.state = GuardState::Invalid;
        guard.attempt = None;
        replace_guard(self, &head, &guard)?;
        head.state = DeliveryState::Invalid;
        head.payload_name = None;
        head.payload_sha256 = None;
        head.output_sha256 = None;
        head.output_bytes = None;
        replace_head(self, hash, &head)
    }

    /// Revalidate the complete captured snapshot and finalize under control locks.
    ///
    /// The supplied audit closure runs while the lineage is locked. A failed
    /// closure leaves the check unresolved for conditional invalidation.
    ///
    /// # Errors
    /// Any supersession or mismatch rejects the original event.
    pub(crate) fn finish_check(
        &self,
        claim: &ClaimedCheck,
        audit: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        let _installation_lock = self.lock()?;
        let active = self.active_locked()?;
        if active.epoch != claim.epoch || active.nonce != claim.nonce {
            return Err(Error::SessionStale);
        }
        let _locks = if let Some(parent) = &claim.parent {
            let locks = self.lock_pair(&parent.lineage.parent_hash, &claim.hash)?;
            self.validate_parent_locked(&active, parent)?;
            (locks.0, Some(locks.1))
        } else {
            (self.directory().lock(&lock_name(&claim.hash))?, None)
        };
        let head = match read_head(self, &claim.hash) {
            Ok(Some(head)) => head,
            Ok(None) => return Err(Error::SnapshotInvalid("head missing")),
            Err(error) => {
                self.poison_locked()?;
                return Err(error);
            }
        };
        if head.epoch != claim.epoch
            || head.nonce != claim.nonce
            || head.sequence != claim.sequence
            || head.generation != claim.generation
            || head.state != DeliveryState::Emitted
            || head.version != if claim.parent.is_some() { 2 } else { 1 }
            || claim
                .parent
                .as_ref()
                .is_some_and(|parent| head.lineage.as_ref() != Some(&parent.lineage))
        {
            return Err(Error::SessionStale);
        }
        let mut guard = read_guard(self, &head)?;
        if guard.state != GuardState::Checking || guard.attempt != Some(claim.attempt) {
            return Err(Error::SessionStale);
        }
        let binding = active.config.resolve(&claim.cwd)?;
        let binding_json = binding.public_json()?;
        if binding_json != claim.binding {
            return Err(Error::SessionStale);
        }
        let root = PrivateDirectory::open(&active.config.state_root, RootMode::Existing)?;
        let pack = validate_payload(
            self,
            &head,
            &root,
            &binding_json,
            binding.project(),
            binding.context(),
        )?;
        if pack != claim.pack {
            return Err(Error::SnapshotInvalid("pack changed"));
        }
        audit().map_err(|_| Error::AuditFailed)?;
        guard.state = GuardState::Completed;
        replace_guard(self, &head, &guard)?;
        let readback = read_guard(self, &head)?;
        if readback.state != GuardState::Completed || readback.attempt != Some(claim.attempt) {
            return Err(Error::SnapshotInvalid("guard completion readback"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod parent_location_tests {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::path::PathBuf;

    use super::{path_from_hex, path_hex};

    #[test]
    fn preserves_bounded_non_utf8_os_paths() {
        let path = PathBuf::from(OsString::from_vec(b"/private/\xff/parent".to_vec()));
        let encoded = path_hex(&path).expect("encode path");
        assert_eq!(path_from_hex(&encoded).expect("decode path"), path);
        assert!(path_hex(&PathBuf::from("relative")).is_err());
        assert!(path_from_hex("2f00").is_err());
        assert!(path_from_hex("2f0").is_err());
        assert!(path_from_hex("2fGG").is_err());
        assert!(path_from_hex("72656c6174697665").is_err());
    }
}
