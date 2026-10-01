//! Closed structured storage registry; no prose inference or override resolver.

use memory_common::guardrails::{GuardrailPack, MAX_CANONICAL_BYTES, MAX_POLICIES};
use memory_common::policy::PolicySelectors;
use serde::Serialize;

use crate::identity::ResolvedBinding;

use super::CandidateRole;
use super::evidence::{PolicyIdentity, Reason, hash};

/// Fixed version-one storage constraint registry.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConstraintKind {
    /// target_storage=persistent-disk, including source-worktree coverage.
    PersistentTarget,
    /// compiler_tmp_storage=persistent-disk.
    PersistentCompilerTemporary,
    /// target_storage=container-local-tmp.
    ContainerTarget,
    /// An unsupported mandate in the storage namespace.
    Unsupported,
}

/// Validated registry entry with authoritative identity and scope status.
#[derive(Clone)]
pub struct StorageRequirement {
    pub(super) identity: PolicyIdentity,
    pub(super) kind: ConstraintKind,
    pub(super) failure: Option<Reason>,
}

impl StorageRequirement {
    /// Authoritative policy identity.
    #[must_use]
    pub const fn identity(&self) -> &PolicyIdentity {
        &self.identity
    }

    /// Exact supported constraint kind.
    #[must_use]
    pub const fn kind(&self) -> ConstraintKind {
        self.kind
    }

    pub(super) const fn applies(&self, role: CandidateRole) -> bool {
        match self.kind {
            ConstraintKind::PersistentTarget => matches!(
                role,
                CandidateRole::TargetDirectory
                    | CandidateRole::BuildOutput
                    | CandidateRole::SourceWorktree
            ),
            ConstraintKind::PersistentCompilerTemporary => {
                matches!(role, CandidateRole::CompilerTemporary)
            }
            ConstraintKind::ContainerTarget => matches!(
                role,
                CandidateRole::TargetDirectory | CandidateRole::BuildOutput
            ),
            ConstraintKind::Unsupported => true,
        }
    }
}

pub(super) fn requirements(
    binding: &ResolvedBinding,
    pack: &GuardrailPack,
) -> Result<Vec<StorageRequirement>, Reason> {
    preflight(binding, pack)?;
    pack.validate_for(binding.project(), binding.context())
        .map_err(|_| Reason::InvalidPack)?;
    let mut requirements = Vec::new();
    for rule in &pack.mandatory {
        for (key, value) in &rule.values {
            if !key.starts_with("rust.build.")
                || !key
                    .split('.')
                    .skip(2)
                    .any(|part| part == "storage" || part.ends_with("_storage"))
            {
                continue;
            }
            let kind = match (key.as_str(), value.as_str()) {
                ("rust.build.target_storage", "persistent-disk") => {
                    ConstraintKind::PersistentTarget
                }
                ("rust.build.compiler_tmp_storage", "persistent-disk") => {
                    ConstraintKind::PersistentCompilerTemporary
                }
                ("rust.build.target_storage", "container-local-tmp") => {
                    ConstraintKind::ContainerTarget
                }
                _ => ConstraintKind::Unsupported,
            };
            let failure = if rule
                .selectors
                .missing_for(binding.context())
                .is_none_or(|missing| !missing.is_empty())
            {
                Some(Reason::SelectorMismatch)
            } else if kind == ConstraintKind::Unsupported {
                Some(Reason::UnsupportedConstraint)
            } else {
                None
            };
            let policy_key = rule.policy_key.as_deref().ok_or(Reason::InvalidPack)?;
            requirements.push(StorageRequirement {
                identity: PolicyIdentity {
                    id: rule.id,
                    key: if kind == ConstraintKind::Unsupported {
                        hash(b"policy-key", policy_key.as_bytes())
                    } else {
                        policy_key.to_owned()
                    },
                    revision: rule.revision.ok_or(Reason::InvalidPack)?,
                    project_hash: hash(b"project", rule.project.as_bytes()),
                },
                kind,
                failure,
            });
        }
    }
    let conflict = requirements
        .iter()
        .any(|r| r.kind == ConstraintKind::PersistentTarget)
        && requirements
            .iter()
            .any(|r| r.kind == ConstraintKind::ContainerTarget);
    if conflict {
        for requirement in &mut requirements {
            if matches!(
                requirement.kind,
                ConstraintKind::PersistentTarget | ConstraintKind::ContainerTarget
            ) {
                requirement.failure = Some(Reason::ConflictingRequirements);
            }
        }
    }
    // Each validated policy has bounded values; cap the expanded assessment set too.
    if requirements.len() > 64 {
        return Err(Reason::Limit);
    }
    Ok(requirements)
}

fn selectors_size(selectors: &PolicySelectors, bytes: &mut usize) -> Result<(), Reason> {
    selectors.validate().map_err(|_| Reason::InvalidPack)?;
    for values in [
        &selectors.profile,
        &selectors.phase,
        &selectors.language,
        &selectors.tool,
    ]
    .into_iter()
    .flatten()
    {
        for value in values {
            add_bytes(bytes, value.len())?;
        }
    }
    Ok(())
}

fn add_bytes(total: &mut usize, length: usize) -> Result<(), Reason> {
    if length > MAX_CANONICAL_BYTES - *total {
        return Err(Reason::Limit);
    }
    *total += length;
    Ok(())
}

fn preflight(binding: &ResolvedBinding, pack: &GuardrailPack) -> Result<(), Reason> {
    // Public decoded fields can be mutated after construction. Bound their raw
    // shape before the shared validator clones or serializes policy metadata.
    // Raw string bytes cannot exceed the shared canonical JSON byte allowance.
    if pack.project != binding.project()
        || &pack.context != binding.context()
        || pack.mandatory.len() > MAX_POLICIES
        || pack.digest.len() != 71
        || pack.digest_algorithm.len() > 64
    {
        return Err(Reason::InvalidPack);
    }
    let mut bytes = 0;
    for rule in &pack.mandatory {
        add_bytes(&mut bytes, rule.project.len())?;
        add_bytes(&mut bytes, rule.content.len())?;
        add_bytes(&mut bytes, rule.policy_key.as_ref().map_or(0, String::len))?;
        selectors_size(&rule.selectors, &mut bytes)?;
        if rule.values.len() > 64 {
            return Err(Reason::InvalidPack);
        }
        for (key, value) in &rule.values {
            add_bytes(&mut bytes, key.len())?;
            add_bytes(&mut bytes, value.len())?;
        }
        if let Some(reference) = &rule.overrides {
            add_bytes(&mut bytes, reference.project.len())?;
            add_bytes(&mut bytes, reference.policy_key.len())?;
            selectors_size(&reference.selectors, &mut bytes)?;
        }
    }
    Ok(())
}
