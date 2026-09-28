//! Generation-scoped `SessionStart` delivery and validated snapshot reads.

use std::io::Write;
use std::path::Path;

use memory_common::guardrails::{GuardrailPack, MAX_PUBLICATION_BYTES};
use memory_common::policy::ResolutionContext;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::config::{ClientAdapter, hash_part};
use crate::error::{Error, Result, io};
use crate::installation::{ActiveInstallation, Installation};
use crate::limits;
use crate::state::{PrivateDirectory, RootMode};

/// Maximum serialized hook output, including worst-case JSON string escaping.
pub const OUTPUT_BYTES: usize = 192 * 1024;

/// The client event whose exact publication envelope was emitted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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
    output_sha256: String,
    output_bytes: u32,
    pack: GuardrailPack,
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
    if head.version != 1
        || head.installation_id != installation.id()
        || head.client != installation.client()
        || head.session_hash != hash
        || head.nonce.is_nil()
        || head.generation.is_nil()
        || head.sequence == 0
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
    if payload.version != 1
        || payload.installation_id != installation.id()
        || payload.client != installation.client()
        || payload.session_hash != head.session_hash
        || payload.epoch != head.epoch
        || payload.nonce != head.nonce
        || payload.generation != head.generation
        || payload.sequence != head.sequence
        || payload.binding != binding
        || head.binding.as_deref() != Some(binding)
        || payload.publication_version != 1
        || head.output_sha256.as_deref() != Some(payload.output_sha256.as_str())
        || head.output_bytes != Some(payload.output_bytes)
    {
        return Err(Error::SnapshotInvalid("payload scope"));
    }
    payload
        .pack
        .validate_for(project, context)
        .map_err(|_| Error::SnapshotInvalid("pack validation"))?;
    let output = exact_output(&payload.pack)?;
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

impl Installation {
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

    /// Retire only the generation observed before an outer worker failure.
    ///
    /// # Errors
    /// A corrupt current head poisons installation authority.
    pub(crate) fn reject_observed_generation(&self, session: &str, generation: Uuid) -> Result<()> {
        let hash = session_hash(self, session)?;
        let _installation_lock = self.lock()?;
        let _session_lock = self.directory().lock(&lock_name(&hash))?;
        match read_head(self, &hash) {
            Ok(Some(head)) if head.generation == generation => retire_current_locked(self, &hash),
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
        let _installation_lock = self.lock()?;
        let active = self.active_locked()?;
        let delivery = active
            .config
            .delivery()
            .ok_or(Error::InstallationInactive)?;
        if active.config.gate().is_none() {
            return Err(Error::ConfigInvalid("gate_contract_required"));
        }
        let _session_lock = self.directory().lock(&lock_name(&hash))?;
        let head = match read_head(self, &hash) {
            Ok(Some(head)) => head,
            Ok(None) => return Err(Error::SnapshotInvalid("head missing")),
            Err(error) => {
                self.poison_locked()?;
                return Err(error);
            }
        };
        let mut guard = match read_guard(self, &head) {
            Ok(guard) => guard,
            Err(error) => {
                retire_current_locked(self, &hash)?;
                return Err(error);
            }
        };
        if guard.state == GuardState::Checking {
            guard.state = GuardState::Invalid;
            guard.attempt = None;
            replace_guard(self, &head, &guard)?;
            let mut invalid = head;
            invalid.state = DeliveryState::Invalid;
            invalid.payload_name = None;
            invalid.payload_sha256 = None;
            invalid.output_sha256 = None;
            invalid.output_bytes = None;
            replace_head(self, &hash, &invalid)?;
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
        let _session_lock = self.directory().lock(&lock_name(&claim.hash))?;
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
