//! Bounded, allowlisted storage evidence and audit handoff.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Point-in-time result; only Pass satisfies an applicable requirement.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Every applicable requirement has positive evidence.
    Pass,
    /// A policy violation was positively observed.
    Deny,
    /// Evidence is incomplete, unsupported, excessive or changed.
    Indeterminate,
    /// No storage requirement applies to this role.
    NotApplicable,
}

impl Outcome {
    pub(super) const fn rank(self) -> u8 {
        match self {
            Self::NotApplicable => 0,
            Self::Pass => 1,
            Self::Indeterminate => 2,
            Self::Deny => 3,
        }
    }
}

/// Safe diagnostic codes; no peer or operating-system messages are retained.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    /// Positive backing evidence satisfies the requirement.
    Verified,
    /// No storage constraint applies.
    Unconstrained,
    /// Pack shape, digest or trusted scope is invalid.
    InvalidPack,
    /// Required selectors are unknown or disagree with the trusted context.
    SelectorMismatch,
    /// Structured storage key or value is unsupported.
    UnsupportedConstraint,
    /// Applicable declarations conflict.
    ConflictingRequirements,
    /// An applicable requirement has no explicit candidate.
    EmptyCandidates,
    /// Candidate index, bytes, or set is outside the contract.
    InvalidCandidate,
    /// The pending operation has no same-namespace/root locality promise.
    UnknownDestination,
    /// Kernel interface or filesystem operation failed.
    Inaccessible,
    /// An existing candidate is not a directory.
    NotDirectory,
    /// The source directory does not exist.
    MissingSource,
    /// A dangling symlink cannot inherit ancestor evidence.
    DanglingSymlink,
    /// Parent traversal follows an unresolved missing component.
    MissingParent,
    /// Procfs magic-link or other special indirection is unsupported.
    SpecialIndirection,
    /// An observation changed; there is no retry or stale Pass.
    Changed,
    /// A cooperative time or operation/size limit was reached.
    Limit,
    /// Mount records are malformed, ambiguous or incoherent.
    MountEvidence,
    /// Positive memory-backed storage evidence violates persistence.
    VolatileBacking,
    /// Filesystem or backing topology cannot prove persistence.
    UnknownBacking,
    /// Required protected container provisioning is absent.
    MissingAttestation,
    /// Protected execution or mount pins positively disagree.
    AttestationMismatch,
    /// The requested or resolved container location is forbidden.
    ForbiddenTarget,
    /// Complete bounded report exceeded the serialization envelope.
    ReportOverflow,
}

/// Supported backing summaries; raw device and mount text remains private.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BackingClass {
    /// All visible device leaves meet the supported disk contract.
    Disk,
    /// tmpfs, ramfs, RAM disk or zram.
    Memory,
    /// An exact protected declaration attests container-local provenance.
    ContainerLocal,
    /// Coherent evidence is unavailable or unsupported.
    Unknown,
}

/// Allowlisted filesystem families.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FilesystemClass {
    /// Linux ext family.
    Ext,
    /// XFS.
    Xfs,
    /// Btrfs, requiring the actual device set.
    Btrfs,
    /// tmpfs or ramfs.
    Memory,
    /// Overlay or union filesystem.
    Overlay,
    /// Everything outside the supported set.
    Other,
}

/// Policy identity associated with each assessment.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PolicyIdentity {
    /// Authoritative policy UUID.
    pub(super) id: Uuid,
    /// Canonical policy key (hash for an unsupported registry entry).
    pub(super) key: String,
    /// Authoritative revision.
    pub(super) revision: i64,
    /// Domain-separated project hash.
    pub(super) project_hash: String,
}

/// Redacted path and backing observations.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Evidence {
    /// Complete requested OS-byte hash.
    pub(super) requested_hash: String,
    /// Complete resolved OS-byte hash, when resolution succeeded.
    pub(super) resolved_hash: Option<String>,
    /// Existing prefix object identity hash.
    pub(super) ancestor_hash: Option<String>,
    /// Exact correlated mount record hash.
    pub(super) mount_hash: Option<String>,
    /// Execution namespace/root/boot hash.
    pub(super) execution_hash: Option<String>,
    /// Whether the destination has an ordinary missing suffix.
    pub(super) missing_suffix: bool,
    /// Allowlisted filesystem family.
    pub(super) filesystem: FilesystemClass,
    /// Allowlisted backing summary.
    pub(super) backing: BackingClass,
    /// Domain-separated digest of the bounded observed backing topology.
    pub(super) backing_hash: Option<String>,
    /// Numeric errno only, if supplied by the provider.
    pub(super) os_code: Option<i32>,
}

/// An explicit candidate/policy assessment; omitted roles are never certified.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Assessment {
    /// Validated authoritative pack digest associated with this assessment.
    pub(super) pack_digest: String,
    /// Caller-selected bounded candidate identifier; None for empty coverage.
    pub(super) candidate_index: Option<u8>,
    /// Candidate role; None for a scope-wide failure.
    pub(super) role: Option<super::CandidateRole>,
    /// Applicable policy; None for `NotApplicable` or invalid scope.
    pub(super) policy: Option<PolicyIdentity>,
    /// Fixed storage registry constraint kind.
    pub(super) constraint: Option<super::policy::ConstraintKind>,
    /// Assessment result.
    pub(super) outcome: Outcome,
    /// Safe reason.
    pub(super) reason: Reason,
    /// Redacted observations.
    pub(super) evidence: Option<Evidence>,
}

/// Version-one report; evaluation never executes or authorizes a command.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Report {
    /// Report contract version.
    pub(super) version: u8,
    /// Deny > Indeterminate > Pass > `NotApplicable`.
    pub(super) outcome: Outcome,
    /// Trusted binding/context digest.
    pub(super) scope_hash: String,
    /// Validated authoritative pack digest.
    pub(super) pack_digest: String,
    /// Complete ordered assessment set, or one compact overflow assessment.
    pub(super) assessments: Vec<Assessment>,
    /// Counts over the complete set, including when overflow is compacted.
    pub(super) counts: [u32; 4],
    /// Digest of every complete assessment before optional compaction.
    pub(super) complete_assessment_hash: String,
    /// Digest of every complete redacted evidence item before compaction.
    pub(super) complete_evidence_hash: String,
}

/// Compact version-one projection for #90; this module never appends it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AuditProjection {
    /// Projection version.
    pub(super) version: u8,
    /// Overall storage result.
    pub(super) outcome: Outcome,
    /// Counts in `NotApplicable`, Pass, Indeterminate, Deny order.
    pub(super) counts: [u32; 4],
    /// Decisive safe reason.
    pub(super) reason: Reason,
    /// Decisive candidate index.
    pub(super) candidate_index: Option<u8>,
    /// Decisive policy UUID.
    pub(super) policy_id: Option<Uuid>,
    /// Decisive policy revision.
    pub(super) policy_revision: Option<i64>,
    /// Remaining decisive identity hash.
    pub(super) policy_hash: Option<String>,
    /// Digest covering every complete assessment and evidence.
    pub(super) assessment_hash: String,
    /// Digest of all redacted evidence.
    pub(super) evidence_hash: String,
}

impl Report {
    /// Produce an allowlisted projection without changing the audit append path.
    ///
    /// # Errors
    /// Returns a safe code if serialization unexpectedly fails.
    pub fn audit_projection(&self) -> Result<AuditProjection, Reason> {
        let decisive = self
            .assessments
            .iter()
            .max_by_key(|item| item.outcome.rank());
        let identity = decisive.and_then(|item| item.policy.as_ref());
        let policy_hash = identity
            .map(|value| serde_json::to_vec(value).map(|bytes| hash(b"policy", &bytes)))
            .transpose()
            .map_err(|_| Reason::ReportOverflow)?;
        Ok(AuditProjection {
            version: 1,
            outcome: self.outcome,
            counts: self.counts,
            reason: decisive.map_or(Reason::Unconstrained, |item| item.reason),
            candidate_index: decisive.and_then(|item| item.candidate_index),
            policy_id: identity.map(|item| item.id),
            policy_revision: identity.map(|item| item.revision),
            policy_hash,
            assessment_hash: self.complete_assessment_hash.clone(),
            evidence_hash: self.complete_evidence_hash.clone(),
        })
    }
}

pub(super) fn hash(domain: &[u8], bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"memory-hooks-storage-v1\0");
    crate::config::hash_part(&mut hasher, domain);
    crate::config::hash_part(&mut hasher, bytes);
    format!("{:x}", hasher.finalize())
}

impl PolicyIdentity {
    /// Authoritative policy UUID.
    #[must_use]
    pub fn id(&self) -> Uuid {
        self.id
    }

    /// Canonical policy key (hash for an unsupported registry entry).
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Authoritative revision.
    #[must_use]
    pub fn revision(&self) -> i64 {
        self.revision
    }

    /// Domain-separated project hash.
    #[must_use]
    pub fn project_hash(&self) -> &str {
        &self.project_hash
    }
}

impl Evidence {
    /// Complete requested OS-byte hash.
    #[must_use]
    pub fn requested_hash(&self) -> &str {
        &self.requested_hash
    }

    /// Complete resolved OS-byte hash, when resolution succeeded.
    #[must_use]
    pub fn resolved_hash(&self) -> Option<&str> {
        self.resolved_hash.as_deref()
    }

    /// Existing prefix object identity hash.
    #[must_use]
    pub fn ancestor_hash(&self) -> Option<&str> {
        self.ancestor_hash.as_deref()
    }

    /// Exact correlated mount record hash.
    #[must_use]
    pub fn mount_hash(&self) -> Option<&str> {
        self.mount_hash.as_deref()
    }

    /// Execution namespace/root/boot hash.
    #[must_use]
    pub fn execution_hash(&self) -> Option<&str> {
        self.execution_hash.as_deref()
    }

    /// Whether the destination has an ordinary missing suffix.
    #[must_use]
    pub fn missing_suffix(&self) -> bool {
        self.missing_suffix
    }

    /// Allowlisted filesystem family.
    #[must_use]
    pub fn filesystem(&self) -> FilesystemClass {
        self.filesystem
    }

    /// Allowlisted backing summary.
    #[must_use]
    pub fn backing(&self) -> BackingClass {
        self.backing
    }

    /// Domain-separated digest of the bounded observed backing topology.
    #[must_use]
    pub fn backing_hash(&self) -> Option<&str> {
        self.backing_hash.as_deref()
    }

    /// Numeric errno only, if supplied by the provider.
    #[must_use]
    pub fn os_code(&self) -> Option<i32> {
        self.os_code
    }
}

impl Assessment {
    /// Validated authoritative pack digest associated with this assessment.
    #[must_use]
    pub fn pack_digest(&self) -> &str {
        &self.pack_digest
    }
    /// Caller-selected bounded candidate identifier; None for empty coverage.
    #[must_use]
    pub fn candidate_index(&self) -> Option<u8> {
        self.candidate_index
    }

    /// Candidate role; None for a scope-wide failure.
    #[must_use]
    pub fn role(&self) -> Option<super::CandidateRole> {
        self.role
    }

    /// Applicable policy; None for `NotApplicable` or invalid scope.
    #[must_use]
    pub fn policy(&self) -> Option<&PolicyIdentity> {
        self.policy.as_ref()
    }

    /// Fixed storage registry constraint kind.
    #[must_use]
    pub fn constraint(&self) -> Option<super::policy::ConstraintKind> {
        self.constraint
    }

    /// Assessment result.
    #[must_use]
    pub fn outcome(&self) -> Outcome {
        self.outcome
    }

    /// Safe reason.
    #[must_use]
    pub fn reason(&self) -> Reason {
        self.reason
    }

    /// Redacted observations.
    #[must_use]
    pub fn evidence(&self) -> Option<&Evidence> {
        self.evidence.as_ref()
    }
}

impl Report {
    /// Report contract version.
    #[must_use]
    pub fn version(&self) -> u8 {
        self.version
    }

    /// Deny > Indeterminate > Pass > `NotApplicable`.
    #[must_use]
    pub fn outcome(&self) -> Outcome {
        self.outcome
    }

    /// Trusted binding/context digest.
    #[must_use]
    pub fn scope_hash(&self) -> &str {
        &self.scope_hash
    }

    /// Validated authoritative pack digest.
    #[must_use]
    pub fn pack_digest(&self) -> &str {
        &self.pack_digest
    }

    /// Complete ordered assessment set, or one compact overflow assessment.
    #[must_use]
    pub fn assessments(&self) -> &[Assessment] {
        &self.assessments
    }

    /// Counts over the complete set, including when overflow is compacted.
    #[must_use]
    pub fn counts(&self) -> [u32; 4] {
        self.counts
    }

    /// Digest of every complete assessment before optional compaction.
    #[must_use]
    pub fn complete_assessment_hash(&self) -> &str {
        &self.complete_assessment_hash
    }

    /// Digest of every complete redacted evidence item before compaction.
    #[must_use]
    pub fn complete_evidence_hash(&self) -> &str {
        &self.complete_evidence_hash
    }
}

impl AuditProjection {
    /// Validate every decoded field before accepting persisted compact evidence.
    /// This does not reconstruct filesystem observations or authorize execution.
    ///
    /// # Errors
    /// Rejects inconsistent outcomes, unsupported versions and invalid bounds.
    pub fn validate(&self) -> Result<(), Reason> {
        let valid_hash = |hash: &str| {
            hash.len() == 64
                && hash
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        };
        let count = self
            .counts
            .iter()
            .try_fold(0_u32, |total, count| total.checked_add(*count));
        let rank = self
            .counts
            .iter()
            .rposition(|count| *count != 0)
            .unwrap_or(0);
        let identity = self.policy_id.is_some();
        let reason_matches = match self.outcome {
            Outcome::Pass => self.reason == Reason::Verified,
            Outcome::NotApplicable => self.reason == Reason::Unconstrained,
            Outcome::Deny => matches!(
                self.reason,
                Reason::VolatileBacking | Reason::AttestationMismatch | Reason::ForbiddenTarget
            ),
            Outcome::Indeterminate => !matches!(
                self.reason,
                Reason::Verified | Reason::Unconstrained | Reason::VolatileBacking
            ),
        };
        if self.version != 1
            || count.is_none_or(|value| value > 4096)
            || (usize::from(self.outcome.rank()) != rank && self.reason != Reason::ReportOverflow)
            || !reason_matches
            || !valid_hash(&self.assessment_hash)
            || !valid_hash(&self.evidence_hash)
            || self
                .candidate_index
                .is_some_and(|index| usize::from(index) >= super::MAX_CANDIDATES)
            || self.policy_id.is_some_and(|id| id.is_nil())
            || self.policy_revision.is_some_and(|revision| revision < 1)
            || identity != self.policy_revision.is_some()
            || identity != self.policy_hash.is_some()
            || self
                .policy_hash
                .as_deref()
                .is_some_and(|hash| !valid_hash(hash))
            || (self.outcome == Outcome::NotApplicable && identity)
        {
            return Err(Reason::InvalidPack);
        }
        Ok(())
    }

    /// Projection version.
    #[must_use]
    pub fn version(&self) -> u8 {
        self.version
    }

    /// Overall storage result.
    #[must_use]
    pub fn outcome(&self) -> Outcome {
        self.outcome
    }

    /// Counts in `NotApplicable`, Pass, Indeterminate, Deny order.
    #[must_use]
    pub fn counts(&self) -> [u32; 4] {
        self.counts
    }

    /// Decisive safe reason.
    #[must_use]
    pub fn reason(&self) -> Reason {
        self.reason
    }

    /// Decisive candidate index.
    #[must_use]
    pub fn candidate_index(&self) -> Option<u8> {
        self.candidate_index
    }

    /// Decisive policy UUID.
    #[must_use]
    pub fn policy_id(&self) -> Option<Uuid> {
        self.policy_id
    }

    /// Decisive policy revision.
    #[must_use]
    pub fn policy_revision(&self) -> Option<i64> {
        self.policy_revision
    }

    /// Remaining decisive identity hash.
    #[must_use]
    pub fn policy_hash(&self) -> Option<&str> {
        self.policy_hash.as_deref()
    }

    /// Digest covering every complete assessment and evidence.
    #[must_use]
    pub fn assessment_hash(&self) -> &str {
        &self.assessment_hash
    }

    /// Digest of all redacted evidence.
    #[must_use]
    pub fn evidence_hash(&self) -> &str {
        &self.evidence_hash
    }
}
