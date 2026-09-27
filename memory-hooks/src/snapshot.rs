//! Generation-scoped `SessionStart` delivery and validated snapshot reads.

use std::io::Write;
use std::path::Path;

use memory_common::guardrails::GuardrailPack;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::config::{ClientAdapter, hash_part};
use crate::error::{Error, Result, io};
use crate::installation::Installation;
use crate::limits;
use crate::state::{PrivateDirectory, RootMode};

/// Maximum serialized hook output, including worst-case JSON string escaping.
pub const OUTPUT_BYTES: usize = 192 * 1024;

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

fn session_hash(installation: &Installation, session: &str) -> Result<String> {
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
    directory.replace(&journal_name(hash), &bytes)
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

fn exact_output(pack: &GuardrailPack) -> Result<Vec<u8>> {
    let publication = pack
        .publication()
        .map_err(|_| Error::SnapshotInvalid("publication"))?;
    let output = HookOutput {
        hook_specific_output: HookSpecificOutput {
            hook_event_name: "SessionStart",
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

impl Installation {
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
        let head = read_head(self, &hash)?.ok_or(Error::SnapshotInvalid("head missing"))?;
        if head.epoch != active.epoch
            || head.nonce != active.nonce
            || head.state != DeliveryState::Emitted
        {
            return Err(Error::SnapshotInvalid("head not current emitted"));
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
}
