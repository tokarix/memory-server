//! Bounded synchronous storage assessment in the caller's Linux namespace.
//!
//! This standalone evaluator does not discover candidates, parse commands,
//! execute tools, fetch policy, append audit records or change gate decisions.
//! A later filesystem change can invalidate its point-in-time result. Async
//! consumers must use `spawn_blocking` and reject expired, late completions.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use memory_common::guardrails::GuardrailPack;
use serde::Serialize;

use crate::identity::ResolvedBinding;

pub(crate) mod attestation;
mod evidence;
mod linux;
mod path;
mod policy;

pub use evidence::{
    Assessment, AuditProjection, BackingClass, Evidence, FilesystemClass, Outcome, PolicyIdentity,
    Reason, Report,
};
pub use policy::{ConstraintKind, StorageRequirement};

/// Query the validated closed registry without probing candidate paths.
/// Unknown storage mandates still require complete Rust execution analysis.
///
/// # Errors
/// Rejects invalid, mismatched or excessive authoritative packs.
pub fn requires_rust_analysis(
    binding: &ResolvedBinding,
    pack: &GuardrailPack,
) -> Result<bool, Reason> {
    Ok(!policy::requirements(binding, pack)?.is_empty())
}

pub(crate) fn validate_rust_requirements(
    binding: &ResolvedBinding,
    pack: &GuardrailPack,
) -> Result<(), Reason> {
    for requirement in policy::requirements(binding, pack)? {
        if let Some(reason) = requirement.failure {
            return Err(reason);
        }
    }
    Ok(())
}

/// Digest the exact protected binding used by a storage decision.
///
/// # Errors
/// Rejects a binding that cannot be represented by its bounded public identity.
pub fn scope_digest(binding: &ResolvedBinding) -> Result<String, Reason> {
    Ok(evidence::hash(
        b"scope",
        binding
            .public_json()
            .map_err(|_| Reason::InvalidPack)?
            .as_bytes(),
    ))
}

#[cfg(test)]
#[derive(Clone, Copy, Default)]
pub(crate) enum FixtureBacking {
    #[default]
    Disk,
    Unknown,
    Race,
    SymlinkEscape,
    NestedMount,
}

#[cfg(test)]
pub(crate) fn fixture_evaluate(
    binding: &ResolvedBinding,
    pack: &GuardrailPack,
    candidates: &[Candidate],
    volatile: Option<CandidateRole>,
    backing: FixtureBacking,
) -> Report {
    tests::gate_report(binding, pack, candidates, volatile, backing)
}

/// Maximum explicit candidates per assessment.
pub const MAX_CANDIDATES: usize = 64;
/// Maximum bytes in a complete redacted report.
pub const MAX_REPORT_BYTES: usize = 128 * 1024;
/// Overall provider operations, including rechecks and backing observations.
pub const MAX_OPERATIONS: usize = 65_536;

/// Version-one directory roles. Individual output files are unsupported.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateRole {
    /// Explicit build target directory.
    TargetDirectory,
    /// Explicit artifact/output directory, never an individual output file.
    BuildOutput,
    /// Explicit compiler temporary directory.
    CompilerTemporary,
    /// Existing source worktree directory.
    SourceWorktree,
}

/// Bounded candidate retaining complete OS bytes privately; no raw Debug.
pub struct Candidate {
    index: u8,
    role: CandidateRole,
    path: PathBuf,
}

impl Candidate {
    /// Construct an explicit directory candidate without probing the filesystem.
    /// Relative paths are resolved against the binding's trusted effective cwd.
    ///
    /// # Errors
    /// Rejects indices above 63, empty paths, NUL bytes and paths above 4 KiB.
    pub fn new(index: u8, role: CandidateRole, path: impl AsRef<OsStr>) -> Result<Self, Reason> {
        let bytes = path.as_ref().as_bytes();
        if usize::from(index) >= MAX_CANDIDATES
            || bytes.is_empty()
            || bytes.len() > crate::limits::PATH_BYTES
            || bytes.contains(&0)
        {
            return Err(Reason::InvalidCandidate);
        }
        Ok(Self {
            index,
            role,
            path: PathBuf::from(path.as_ref()),
        })
    }

    /// Caller-selected bounded identifier.
    #[must_use]
    pub const fn index(&self) -> u8 {
        self.index
    }
    /// Explicit directory role.
    #[must_use]
    pub const fn role(&self) -> CandidateRole {
        self.role
    }
}

/// Locality promise that #90 must substantiate before using an assessment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionDestination {
    /// The pending operation will use this evaluator's mount namespace/root.
    SameNamespaceAndRoot,
    /// Remote, future container entry, chroot/setns, or unknown destination.
    Unknown,
}

/// Trusted policy scope and held locally observed execution descriptors.
/// No project/profile setter or client-selectable provider is exposed.
pub struct EvaluationScope {
    engine: Engine<linux::Linux>,
}

struct Engine<P> {
    cwd: PathBuf,
    profile: Option<String>,
    scope_hash: String,
    pack_digest: String,
    requirements: Vec<StorageRequirement>,
    provider: P,
    initial: linux::Snapshot,
    attestation: Option<attestation::StorageExecution>,
    budget: Budget,
}

/// Assess explicit paths against trusted structured policy in this namespace.
/// Scope failures return compact redacted reports without path probes.
/// The caller must substantiate the locality promise before using this result.
#[must_use]
pub fn evaluate(
    binding: &ResolvedBinding,
    pack: &GuardrailPack,
    destination: ExecutionDestination,
    candidates: &[Candidate],
) -> Report {
    if candidates.len() <= MAX_CANDIDATES
        && let Ok(requirements) = policy::requirements(binding, pack)
    {
        let applicable = if candidates.is_empty() {
            !requirements.is_empty()
        } else {
            candidates.iter().any(|candidate| {
                requirements
                    .iter()
                    .any(|requirement| requirement.applies(candidate.role))
            })
        };
        let indices = candidates
            .iter()
            .map(|candidate| candidate.index)
            .collect::<BTreeSet<_>>();
        if !applicable && candidates.len() <= MAX_CANDIDATES && indices.len() == candidates.len() {
            let assessments = candidates
                .iter()
                .map(|candidate| {
                    let mut assessment = failure(
                        Some(candidate.index),
                        Some(candidate.role),
                        None,
                        Reason::Unconstrained,
                    );
                    assessment.outcome = Outcome::NotApplicable;
                    assessment
                })
                .collect();
            return finish(Report {
                version: 1,
                outcome: Outcome::NotApplicable,
                scope_hash: evidence::hash(
                    b"scope",
                    binding.public_json().unwrap_or_default().as_bytes(),
                ),
                pack_digest: pack.digest.clone(),
                assessments,
                counts: [0; 4],
                complete_assessment_hash: String::new(),
                complete_evidence_hash: String::new(),
            });
        }
    }
    match EvaluationScope::observe(binding, pack, destination) {
        Ok(scope) => scope.evaluate(candidates),
        Err(reason) => {
            let requirements = policy::requirements(binding, pack);
            let validated = requirements.is_ok();
            let assessments = if let Ok(requirements) = requirements {
                if reason == Reason::SelectorMismatch {
                    requirements
                        .iter()
                        .map(|requirement| {
                            failure(
                                None,
                                None,
                                Some(requirement),
                                requirement.failure.unwrap_or(reason),
                            )
                        })
                        .collect()
                } else {
                    vec![failure(None, None, None, reason)]
                }
            } else {
                vec![failure(None, None, None, reason)]
            };
            finish(Report {
                version: 1,
                outcome: Outcome::Indeterminate,
                scope_hash: evidence::hash(b"scope", binding.fingerprint().as_bytes()),
                pack_digest: if validated {
                    pack.digest.clone()
                } else {
                    String::new()
                },
                assessments,
                counts: [0; 4],
                complete_assessment_hash: String::new(),
                complete_evidence_hash: String::new(),
            })
        }
    }
}

impl EvaluationScope {
    /// Validate scope before probing, then pin this process's execution identity.
    /// Call immediately before `evaluate`; the cooperative budget includes both.
    ///
    /// # Errors
    /// Returns allowlisted codes for invalid policy/locality or missing evidence.
    pub fn observe(
        binding: &ResolvedBinding,
        pack: &GuardrailPack,
        destination: ExecutionDestination,
    ) -> Result<Self, Reason> {
        Self::observe_before(
            binding,
            pack,
            destination,
            Instant::now() + Duration::from_secs(5),
        )
    }

    /// Observe within the caller's original deadline and the storage budget.
    ///
    /// # Errors
    /// Returns fixed scope, locality, observation or deadline failure codes.
    pub fn observe_before(
        binding: &ResolvedBinding,
        pack: &GuardrailPack,
        destination: ExecutionDestination,
        deadline: Instant,
    ) -> Result<Self, Reason> {
        let requirements = policy::requirements(binding, pack)?;
        if requirements
            .iter()
            .any(|requirement| requirement.failure == Some(Reason::SelectorMismatch))
        {
            return Err(Reason::SelectorMismatch);
        }
        if destination != ExecutionDestination::SameNamespaceAndRoot {
            return Err(Reason::UnknownDestination);
        }
        let mut budget = Budget::new();
        budget.deadline = budget.deadline.min(deadline);
        let mut provider = linux::Linux::new(&mut budget).map_err(|error| error.reason)?;
        let initial = provider
            .snapshot(&mut budget)
            .map_err(|error| error.reason)?;
        Ok(Self {
            engine: Engine {
                cwd: binding.effective_cwd().to_owned(),
                profile: binding.context().profile.clone(),
                scope_hash: evidence::hash(
                    b"scope",
                    binding
                        .public_json()
                        .map_err(|_| Reason::InvalidPack)?
                        .as_bytes(),
                ),
                pack_digest: pack.digest.clone(),
                requirements,
                provider,
                initial,
                attestation: binding.storage_execution().cloned(),
                budget,
            },
        })
    }

    /// Applicable authoritative registry entries; no winner is selected here.
    #[must_use]
    pub fn requirements(&self) -> &[StorageRequirement] {
        &self.engine.requirements
    }

    /// Observe explicit candidates without writing or executing anything.
    #[must_use]
    pub fn evaluate(self, candidates: &[Candidate]) -> Report {
        self.engine.evaluate(candidates)
    }
}

impl<P: linux::Kernel> Engine<P> {
    #[expect(
        clippy::too_many_lines,
        reason = "bounded assessment and fail-closed final rechecks"
    )]
    fn evaluate(mut self, candidates: &[Candidate]) -> Report {
        let mut report = Report {
            version: 1,
            outcome: Outcome::NotApplicable,
            scope_hash: self.scope_hash,
            pack_digest: self.pack_digest,
            assessments: Vec::new(),
            counts: [0; 4],
            complete_assessment_hash: String::new(),
            complete_evidence_hash: String::new(),
        };
        let mut seen = BTreeSet::new();
        let mut retained = Vec::new();
        if candidates.len() > MAX_CANDIDATES
            || candidates
                .iter()
                .any(|candidate| !seen.insert(candidate.index))
        {
            report
                .assessments
                .push(failure(None, None, None, Reason::InvalidCandidate));
        } else if candidates.is_empty() && !self.requirements.is_empty() {
            for requirement in &self.requirements {
                report.assessments.push(failure(
                    None,
                    None,
                    Some(requirement),
                    requirement.failure.unwrap_or(Reason::EmptyCandidates),
                ));
            }
        } else {
            let mut ordered = candidates.iter().collect::<Vec<_>>();
            ordered.sort_by_key(|candidate| candidate.index);
            for candidate in ordered {
                let applicable = self
                    .requirements
                    .iter()
                    .filter(|requirement| requirement.applies(candidate.role))
                    .collect::<Vec<_>>();
                if applicable.is_empty() {
                    let mut item = failure(
                        Some(candidate.index),
                        Some(candidate.role),
                        None,
                        Reason::Unconstrained,
                    );
                    item.outcome = Outcome::NotApplicable;
                    report.assessments.push(item);
                    continue;
                }
                let observation = if applicable
                    .iter()
                    .all(|requirement| requirement.failure.is_some())
                {
                    None
                } else {
                    Some(path::resolve(
                        &mut self.provider,
                        &mut self.budget,
                        &self.cwd,
                        candidate,
                    ))
                };
                for requirement in applicable {
                    let mut item = failure(
                        Some(candidate.index),
                        Some(candidate.role),
                        Some(requirement),
                        Reason::UnknownBacking,
                    );
                    if let Some(reason) = requirement.failure {
                        item.reason = reason;
                    } else if let Some(observation) = &observation {
                        match observation {
                            Err(error) => {
                                item.reason = error.reason;
                                item.evidence = Some(empty_evidence(candidate, error.code));
                            }
                            Ok(resolved) => {
                                let evaluated = linux::assess(
                                    &mut self.provider,
                                    &mut self.budget,
                                    &self.initial,
                                    resolved,
                                    candidate,
                                    requirement,
                                    self.profile.as_deref(),
                                    self.attestation.as_ref(),
                                );
                                match evaluated {
                                    Ok((outcome, reason, evidence)) => {
                                        item.outcome = outcome;
                                        item.reason = reason;
                                        item.evidence = Some(evidence);
                                    }
                                    Err(error) => {
                                        item.reason = error.reason;
                                        item.evidence = Some(empty_evidence(candidate, error.code));
                                    }
                                }
                            }
                        }
                    }
                    report.assessments.push(item);
                }
                if let Some(Ok(resolved)) = observation {
                    retained.push(resolved);
                }
            }
        }
        for resolved in retained {
            if let Err(error) = resolved.recheck(&mut self.provider, &mut self.budget) {
                invalidate(&mut report, error.reason);
            }
        }
        if let Err(error) = self.provider.recheck(&self.initial, &mut self.budget) {
            invalidate(&mut report, error.reason);
        }
        finish(report)
    }
}

fn finish(mut report: Report) -> Report {
    for assessment in &mut report.assessments {
        assessment.pack_digest.clone_from(&report.pack_digest);
    }
    report
        .assessments
        .sort_by_key(|assessment| assessment.candidate_index);
    report.outcome = report
        .assessments
        .iter()
        .map(|item| item.outcome)
        .max_by_key(|outcome| outcome.rank())
        .unwrap_or(Outcome::NotApplicable);
    for assessment in &report.assessments {
        report.counts[usize::from(assessment.outcome.rank())] += 1;
    }
    report.complete_assessment_hash = serde_json::to_vec(&(
        report.version,
        &report.scope_hash,
        &report.pack_digest,
        &report.assessments,
    ))
    .map_or_else(
        |_| evidence::hash(b"assessment", b"serialization-failed"),
        |bytes| evidence::hash(b"assessment", &bytes),
    );
    let evidence = report
        .assessments
        .iter()
        .map(|item| &item.evidence)
        .collect::<Vec<_>>();
    report.complete_evidence_hash = serde_json::to_vec(&evidence).map_or_else(
        |_| evidence::hash(b"evidence", b"serialization-failed"),
        |bytes| evidence::hash(b"evidence", &bytes),
    );
    if serde_json::to_vec(&report).is_ok_and(|bytes| bytes.len() <= MAX_REPORT_BYTES) {
        return report;
    }
    // Never truncate away a violation. Overflow cannot satisfy any requirement.
    let mut compact = report
        .assessments
        .iter()
        .max_by_key(|item| item.outcome.rank())
        .cloned()
        .unwrap_or_else(|| failure(None, None, None, Reason::ReportOverflow));
    compact.outcome = Outcome::Indeterminate;
    compact.reason = Reason::ReportOverflow;
    report.outcome = Outcome::Indeterminate;
    report.assessments = vec![compact];
    report
}

fn invalidate(report: &mut Report, reason: Reason) {
    for item in &mut report.assessments {
        if item.outcome != Outcome::NotApplicable {
            item.outcome = Outcome::Indeterminate;
            item.reason = reason;
        }
    }
}

fn failure(
    index: Option<u8>,
    role: Option<CandidateRole>,
    requirement: Option<&StorageRequirement>,
    reason: Reason,
) -> Assessment {
    Assessment {
        pack_digest: String::new(),
        candidate_index: index,
        role,
        policy: requirement.map(|r| r.identity.clone()),
        constraint: requirement.map(|r| r.kind),
        outcome: Outcome::Indeterminate,
        reason,
        evidence: None,
    }
}

fn empty_evidence(candidate: &Candidate, code: Option<i32>) -> Evidence {
    Evidence {
        requested_hash: evidence::hash(b"requested", candidate.path.as_os_str().as_bytes()),
        resolved_hash: None,
        ancestor_hash: None,
        mount_hash: None,
        execution_hash: None,
        missing_suffix: false,
        filesystem: FilesystemClass::Other,
        backing: BackingClass::Unknown,
        backing_hash: None,
        os_code: code,
    }
}

struct Budget {
    start: Instant,
    deadline: Instant,
    operations: usize,
}
impl Budget {
    fn new() -> Self {
        Self {
            start: Instant::now(),
            deadline: Instant::now() + Duration::from_secs(5),
            operations: 0,
        }
    }
    fn tick(&mut self) -> ProbeResult<()> {
        self.operations += 1;
        if self.operations > MAX_OPERATIONS
            || self.start.elapsed() >= Duration::from_secs(5)
            || Instant::now() >= self.deadline
        {
            return Err(ProbeError::new(Reason::Limit));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProbeError {
    reason: Reason,
    code: Option<i32>,
}
impl ProbeError {
    const fn new(reason: Reason) -> Self {
        Self { reason, code: None }
    }
}
impl From<rustix::io::Errno> for ProbeError {
    fn from(error: rustix::io::Errno) -> Self {
        Self {
            reason: Reason::Inaccessible,
            code: Some(error.raw_os_error()),
        }
    }
}
impl From<std::io::Error> for ProbeError {
    fn from(error: std::io::Error) -> Self {
        Self {
            reason: Reason::Inaccessible,
            code: error.raw_os_error(),
        }
    }
}
type ProbeResult<T> = Result<T, ProbeError>;

#[cfg(test)]
mod tests;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
struct ObjectIdentity {
    device: u64,
    inode: u64,
    mode: u32,
}

enum Entry<H> {
    Directory(H, ObjectIdentity),
    Symlink(H, ObjectIdentity),
    Other,
}

// Private testing seam; clients cannot supply observed facts or a provider.
trait Provider {
    type Handle: Clone;
    fn root(&mut self, budget: &mut Budget) -> ProbeResult<Self::Handle>;
    fn entry(
        &mut self,
        parent: &Self::Handle,
        name: &OsStr,
        budget: &mut Budget,
    ) -> ProbeResult<Entry<Self::Handle>>;
    fn link(&mut self, handle: &Self::Handle, budget: &mut Budget) -> ProbeResult<Vec<u8>>;
    fn accessible(&mut self, handle: &Self::Handle, budget: &mut Budget) -> ProbeResult<()>;
    fn identity(
        &mut self,
        handle: &Self::Handle,
        budget: &mut Budget,
    ) -> ProbeResult<ObjectIdentity>;
}
