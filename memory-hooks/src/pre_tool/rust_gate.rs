//! Synchronous claim-bound Rust analysis, storage and mandatory audit handoff.

use std::time::Instant;

use crate::audit::{self, RustEvidence, RustOutcome};
use crate::config::{GateConfig, TrustedHooksConfig};
use crate::error::{Error, Result};
use crate::identity::ResolvedBinding;
use crate::installation::Installation;
use crate::rust_build::Action;
use crate::rust_build::command;
use crate::rust_build::files::Filesystem;
use crate::rust_build::files::ReadProvider;
use crate::rust_build::operation::Operation;
use crate::snapshot::ClaimedCheck;
use crate::storage::{self, EvaluationScope, ExecutionDestination, Outcome};

use super::{DenyReason, PreToolEvent, ToolCategory};

trait Runtime {
    type Reader: ReadProvider;
    fn reader(&self) -> &Self::Reader;
    fn locality(
        &self,
        contract: &crate::config::execution::RustExecution,
        deadline: Instant,
    ) -> Result<()>;
    fn evaluate(
        &self,
        binding: &ResolvedBinding,
        claim: &ClaimedCheck,
        candidates: &[storage::Candidate],
        deadline: Instant,
    ) -> Result<storage::Report>;
}

struct LocalRuntime(Filesystem);

impl Runtime for LocalRuntime {
    type Reader = Filesystem;
    fn reader(&self) -> &Filesystem {
        &self.0
    }
    fn locality(
        &self,
        contract: &crate::config::execution::RustExecution,
        deadline: Instant,
    ) -> Result<()> {
        contract
            .verify_locality(deadline)
            .map_err(|_| Error::RustExecutionUnsupported)
    }
    fn evaluate(
        &self,
        binding: &ResolvedBinding,
        claim: &ClaimedCheck,
        candidates: &[storage::Candidate],
        deadline: Instant,
    ) -> Result<storage::Report> {
        let scope = EvaluationScope::observe_before(
            binding,
            &claim.pack,
            ExecutionDestination::SameNamespaceAndRoot,
            deadline,
        )
        .map_err(|_| Error::RustAnalysisIndeterminate)?;
        Ok(scope.evaluate(candidates))
    }
}

pub(super) struct Failure {
    pub reason: DenyReason,
}

pub(super) struct Check<'a> {
    pub installation: &'a Installation,
    pub config: &'a TrustedHooksConfig,
    pub claim: &'a ClaimedCheck,
    pub event: &'a PreToolEvent,
    pub input: &'a [u8],
    pub deadline: Instant,
    pub gate: GateConfig,
    pub spawn: bool,
}

fn reason(error: Error) -> DenyReason {
    match error {
        Error::RustExecutionUnsupported => DenyReason::RustExecutionUnsupported,
        Error::RustAnalysisIndeterminate => DenyReason::RustAnalysisIndeterminate,
        Error::RustStorageDenied => DenyReason::RustStorageDenied,
        Error::RustStorageIndeterminate => DenyReason::RustStorageIndeterminate,
        Error::AuditFailed => DenyReason::AuditFailed,
        _ => DenyReason::SnapshotInvalid,
    }
}

fn audit_before(deadline: Instant, persist: impl FnOnce() -> Result<()>) -> Result<()> {
    if Instant::now() >= deadline {
        return Err(Error::RustAnalysisIndeterminate);
    }
    persist().map_err(|_| Error::AuditFailed)?;
    // Durable append also consumes the original operation budget. A late
    // append cannot complete the claim, even if its preceding checks passed.
    if Instant::now() >= deadline {
        return Err(Error::RustAnalysisIndeterminate);
    }
    Ok(())
}

impl Check<'_> {
    fn generic(&self) -> Result<()> {
        self.installation.finish_check(self.claim, || {
            audit::append(
                self.installation,
                self.claim,
                self.event.category(),
                self.spawn,
                "neutral",
                "checked",
                self.gate,
            )
        })
    }

    fn analyze<R: Runtime>(
        &self,
        callback: &ResolvedBinding,
        evidence: &mut RustEvidence,
        runtime: &R,
    ) -> Result<()> {
        let contract = self
            .config
            .rust_execution()
            .ok_or(Error::RustExecutionUnsupported)?;
        if self.event.category() != ToolCategory::Shell {
            return Err(Error::RustExecutionUnsupported);
        }
        // Never adopt the hook's ambient environment as execution evidence.
        runtime.locality(contract, self.deadline)?;
        let input = self
            .event
            .execution()
            .ok_or(Error::RustAnalysisIndeterminate)?;
        let invocation =
            command::parse(input, contract).map_err(|_| Error::RustAnalysisIndeterminate)?;
        storage::validate_rust_requirements(callback, &self.claim.pack)
            .map_err(|_| Error::RustAnalysisIndeterminate)?;
        let checked = self
            .config
            .resolve_execution(callback, invocation.cwd(), self.deadline)
            .map_err(|_| Error::RustExecutionUnsupported)?;
        let operation = Operation::resolve(
            checked.binding().effective_cwd(),
            &invocation,
            contract,
            runtime.reader(),
            self.deadline,
        )
        .map_err(|_| Error::RustAnalysisIndeterminate)?;
        let candidates = operation
            .candidates()
            .map_err(|_| Error::RustAnalysisIndeterminate)?;
        *evidence = RustEvidence::new(
            checked.binding(),
            self.claim,
            &operation.digest(),
            contract.version(),
        )?;
        evidence.analyzed(
            match operation.action() {
                Action::AuditedReadOnly => RustOutcome::AuditedReadOnly,
                Action::Build(_) => RustOutcome::CompleteBuild,
                Action::KnownNonExecution => return Err(Error::RustAnalysisIndeterminate),
            },
            candidates.len(),
        )?;
        self.installation.finish_checked(self.claim, |config| {
            let final_contract = config
                .rust_execution()
                .ok_or(Error::RustExecutionUnsupported)?;
            checked
                .recheck(config)
                .map_err(|_| Error::RustExecutionUnsupported)?;
            runtime.locality(final_contract, self.deadline)?;
            operation
                .recheck(final_contract)
                .map_err(|_| Error::RustAnalysisIndeterminate)?;
            if matches!(operation.action(), Action::Build(_)) {
                // Every complete build retains every required role, even when
                // paths coincide. A passing target cannot conceal missing temp.
                for role in [
                    storage::CandidateRole::TargetDirectory,
                    storage::CandidateRole::BuildOutput,
                    storage::CandidateRole::CompilerTemporary,
                    storage::CandidateRole::SourceWorktree,
                ] {
                    if !candidates.iter().any(|candidate| candidate.role() == role) {
                        return Err(Error::RustAnalysisIndeterminate);
                    }
                }
                let report =
                    runtime.evaluate(checked.binding(), self.claim, &candidates, self.deadline)?;
                evidence.attach(&report).map_err(|_| Error::AuditFailed)?;
                match report.outcome() {
                    Outcome::Pass => {}
                    Outcome::Deny => return Err(Error::RustStorageDenied),
                    Outcome::Indeterminate => return Err(Error::RustStorageIndeterminate),
                    Outcome::NotApplicable => return Err(Error::RustAnalysisIndeterminate),
                }
            }
            // The retained original deadline, input absences and executable
            // identities are checked again immediately before durable audit.
            operation
                .recheck(final_contract)
                .map_err(|_| Error::RustAnalysisIndeterminate)?;
            checked
                .recheck(config)
                .map_err(|_| Error::RustExecutionUnsupported)?;
            runtime.locality(final_contract, self.deadline)?;
            audit_before(self.deadline, || {
                audit::append_rust(
                    self.installation,
                    self.claim,
                    self.event.category(),
                    "neutral",
                    "checked",
                    self.gate,
                    Some(evidence.clone()),
                )
            })
        })
    }

    pub(super) fn finish(&self) -> std::result::Result<(), Failure> {
        self.finish_using(&LocalRuntime(Filesystem))
    }

    fn finish_using<R: Runtime>(&self, runtime: &R) -> std::result::Result<(), Failure> {
        let mut audited = false;
        let result = self.config.resolve(self.event.cwd()).and_then(|callback| {
            if callback.public_json()? != self.claim.binding {
                return Err(Error::SessionStale);
            }
            let enforced = storage::requires_rust_analysis(&callback, &self.claim.pack)
                .map_err(|_| Error::SnapshotInvalid("storage registry"))?;
            if !enforced
                || matches!(
                    self.event.category(),
                    ToolCategory::Write | ToolCategory::Read | ToolCategory::Agent
                )
            {
                // Literal client cwd still cannot silently select a new binding.
                if let Some(input) = self.event.execution() {
                    self.config
                        .resolve_execution(&callback, input.cwd(), self.deadline)?;
                }
                return self.generic();
            }
            let version = self
                .config
                .rust_execution()
                .map_or(0, crate::config::execution::RustExecution::version);
            let mut evidence = RustEvidence::new(&callback, self.claim, self.input, version)?;
            match self.analyze(&callback, &mut evidence, runtime) {
                Ok(()) => Ok(()),
                Err(error) => {
                    let reason = reason(error);
                    if matches!(
                        error,
                        Error::RustAnalysisIndeterminate | Error::RustExecutionUnsupported
                    ) {
                        // Never persist stale complete/pass evidence for a failed
                        // final read-set, executable, locality or deadline check.
                        evidence = RustEvidence::new(&callback, self.claim, self.input, version)?;
                    }
                    let evidence = matches!(
                        error,
                        Error::RustAnalysisIndeterminate
                            | Error::RustExecutionUnsupported
                            | Error::RustStorageDenied
                            | Error::RustStorageIndeterminate
                    )
                    .then_some(evidence);
                    audited = true;
                    let audit_result = audit::append_rust(
                        self.installation,
                        self.claim,
                        self.event.category(),
                        "deny",
                        reason.code(),
                        self.gate,
                        evidence,
                    );
                    Err(if audit_result.is_err() {
                        Error::AuditFailed
                    } else {
                        error
                    })
                }
            }
        });
        result.map_err(|error| {
            let mut denial = reason(error);
            if !audited
                && audit::append(
                    self.installation,
                    self.claim,
                    self.event.category(),
                    self.spawn,
                    "deny",
                    denial.code(),
                    self.gate,
                )
                .is_err()
            {
                denial = DenyReason::AuditFailed;
            }
            Failure { reason: denial }
        })
    }
}

#[cfg(test)]
mod tests;
