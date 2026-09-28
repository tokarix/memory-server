//! Explicit advisory handling for child startup events without a verified pin.

use std::io::Write;
use std::time::{Duration, Instant};

use crate::error::Result;
use crate::installation::Installation;
use crate::session_identity::parse_delegated_start;
use crate::session_start::read_event_bytes;

/// Consume a typed child lifecycle callback under a root-only installation.
///
/// This command deliberately emits no guardrail context: client capability
/// evidence does not replace the missing child-bound snapshot transaction.
/// The client cannot block child creation from this startup hook. Covered child
/// mutations are denied by the separate pre-tool boundary.
///
/// # Errors
/// Rejects malformed, oversized, inactive or wrong-client callbacks without
/// selecting or retiring the enclosing root snapshot.
pub fn run_advisory(installation: &Installation) -> Result<()> {
    let bytes = read_event_bytes(Instant::now() + Duration::from_secs(60))?;
    let _start = parse_delegated_start(installation.client(), &bytes)?;
    let _active = installation.active()?;
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
