//! Managed direct-child startup and explicit unsupported lifecycle handling.

use std::io::Write;
use std::time::{Duration, Instant};

use memory_common::http_client::HttpMemoryClient;

use crate::audit;
use crate::config::{ClientAdapter, DelegationConfig};
use crate::error::{Error, Result};
use crate::installation::Installation;
use crate::pre_tool::{IdentifiableIdentity, identifiable_identity};
use crate::session_identity::parse_delegated_start;
use crate::session_start::{DeadlineWriter, read_event_bytes};

// The pinned Claude context path has a smaller reliable inline envelope than
// the shared pack limit. A spilled preview cannot establish child delivery.
const INLINE_CHILD_CONTEXT_BYTES: usize = 10_000;

fn diagnosed<T>(status: &'static str, result: Result<T>) -> Result<T> {
    if let Err(error) = &result {
        eprintln!("delegated_start: status={status} code={}", error.code());
    }
    result
}

/// Consume a typed child lifecycle callback for an exact managed contract.
///
/// Enforced Claude direct-child mode publishes a separate generation after
/// fresh parent validation. Advisory mode emits no context. The client cannot
/// block child creation from this callback in either mode.
///
/// # Errors
/// Rejects malformed, oversized, inactive or wrong-client callbacks without
/// borrowing the enclosing root snapshot as child authority.
pub fn run(installation: &Installation) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(90);
    let bytes = read_event_bytes(deadline)?;
    let start = match parse_delegated_start(installation.client(), &bytes) {
        Ok(start) => start,
        Err(error) => {
            if let Some(IdentifiableIdentity::Child(child)) = identifiable_identity(&bytes) {
                let _ = installation.reject_event_child(&child);
            }
            return Err(error);
        }
    };
    let active = installation.active()?;
    if active
        .config
        .delegation()
        .is_some_and(DelegationConfig::enforced)
    {
        return run_enforced(installation, &start, deadline);
    }
    if installation.client() == ClientAdapter::CodexV1 {
        installation.mark_ambiguous(start.identity().session_id())?;
    }
    if let Some(gate) = active.config.gate() {
        audit::append_unsupported_start(installation, start.identity(), gate)
            .map_err(|_| Error::AuditFailed)?;
    }
    let mut stderr = std::io::stderr().lock();
    writeln!(
        stderr,
        "delegated_start: status=advisory_only code=unsupported_capability"
    )
    .map_err(|error| crate::error::io("advisory diagnostic", &error))?;
    stderr
        .flush()
        .map_err(|error| crate::error::io("advisory diagnostic", &error))
}

fn run_enforced(
    installation: &Installation,
    start: &crate::session_identity::DelegatedStart,
    deadline: Instant,
) -> Result<()> {
    let pending = diagnosed(
        "unsupported_event",
        installation.begin_child(start.identity()),
    )?;
    let proof = diagnosed(
        "parent_invalid",
        installation.capture_parent(start.identity().session_id(), start.cwd()),
    )?;
    let child_binding = diagnosed(
        "scope_mismatch",
        installation.attach_binding(&pending, start.cwd()),
    )?;
    if child_binding.project() != proof.project || child_binding.context() != &proof.context {
        return diagnosed(
            "scope_mismatch",
            Err(Error::SnapshotInvalid("child scope mismatch")),
        );
    }
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .map_err(|_| Error::GuardrailsUnavailable("client setup"))?;
    let client = HttpMemoryClient::with_http_client(&proof.origin, proof.api_token.clone(), http)
        .map_err(|_| Error::GuardrailsUnavailable("client setup"))?
        .with_context(proof.context.clone());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| Error::GuardrailsUnavailable("runtime"))?;
    let fetched = runtime.block_on(client.guardrails(&proof.project));
    let fresh = match fetched {
        Ok(pack) if pack.validate_for(&proof.project, &proof.context).is_ok() => pack,
        _ => {
            let _ = installation
                .reject_observed_generation(start.identity().session_id(), proof.generation());
            return diagnosed(
                "parent_invalid",
                Err(Error::GuardrailsUnavailable("parent refresh")),
            );
        }
    };
    if fresh != proof.pack {
        let _ = installation
            .reject_observed_generation(start.identity().session_id(), proof.generation());
        return diagnosed(
            "parent_invalid",
            Err(Error::SnapshotInvalid("parent policy changed")),
        );
    }
    if proof
        .pack
        .publication()
        .map_err(|_| Error::SnapshotInvalid("child publication"))?
        .len()
        > INLINE_CHILD_CONTEXT_BYTES
    {
        return diagnosed("publication_failed", Err(Error::OutputTooLarge));
    }
    let mut writer = DeadlineWriter::new(deadline)
        .map_err(|error| crate::error::io("child output flags", &error))?;
    diagnosed(
        "publication_failed",
        installation.complete_child(start.identity(), &pending, &proof, &mut writer),
    )
}
