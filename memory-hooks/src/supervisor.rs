//! Supervised pre-tool command and bounded worker result protocol.

use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use uuid::Uuid;

use crate::audit;
use crate::config::ClientAdapter;
use crate::error::{Error, Result};
use crate::installation::Installation;
use crate::pre_tool::{
    DenyReason, IdentifiableIdentity, PreToolEvent, WorkerMessage, deny_json,
    identifiable_identity, parse_event, run_worker,
};
use crate::session_start::{DeadlineWriter, read_event_bytes};

// Reserve time for conditional retirement, required denial audit and output.
const DEADLINE: Duration = Duration::from_secs(35);
const RESPONSE_BYTES: usize = 2048;
type ParsedResponse = (Option<(Uuid, Uuid)>, DenyReason, bool);

fn arguments() -> Result<(PathBuf, Uuid, ClientAdapter)> {
    let worker =
        std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("pre-tool-worker"));
    let mut args = std::env::args_os().skip(2);
    let mut installation = None;
    let mut id = None;
    let mut client = None;
    let mut worker_key = None;
    while let Some(name) = args.next() {
        let value = args
            .next()
            .ok_or(Error::EventInvalid("launcher arguments"))?;
        if name == "--installation" && installation.is_none() {
            installation = Some(PathBuf::from(value));
        } else if name == "--installation-id" && id.is_none() {
            id = value.to_str().and_then(|s| Uuid::parse_str(s).ok());
        } else if name == "--client" && client.is_none() {
            client = value.to_str().and_then(ClientAdapter::from_name);
        } else if name == "--worker-key" && worker && worker_key.is_none() {
            worker_key = value.to_str().and_then(|s| Uuid::parse_str(s).ok());
        } else {
            return Err(Error::EventInvalid("launcher arguments"));
        }
    }
    if worker {
        let supplied = worker_key.ok_or(Error::EventInvalid("worker key"))?;
        let inherited = std::env::var("MEMORY_HOOKS_WORKER_KEY")
            .ok()
            .and_then(|value| Uuid::parse_str(&value).ok());
        if supplied.is_nil() || inherited != Some(supplied) {
            return Err(Error::EventInvalid("worker key"));
        }
    }
    Ok((
        installation.ok_or(Error::EventInvalid("installation"))?,
        id.ok_or(Error::EventInvalid("installation id"))?,
        client.ok_or(Error::EventInvalid("client"))?,
    ))
}

fn map_error(error: Error) -> DenyReason {
    match error {
        Error::EventInvalid(_) | Error::EventTooLarge => DenyReason::EventInvalid,
        Error::ConfigInvalid("gate_contract_required") => DenyReason::GateContractRequired,
        Error::GuardrailsUnavailable(_) => DenyReason::GuardrailsUnavailable,
        Error::AuditFailed => DenyReason::AuditFailed,
        Error::InstallationInactive | Error::SessionStale => DenyReason::FreshSessionRequired,
        Error::UnsupportedPlatform => DenyReason::UnsupportedCapability,
        Error::SnapshotInvalid("child head missing") => DenyReason::MissingChildLinkage,
        Error::SnapshotInvalid("parent missing" | "parent location" | "parent changed") => {
            DenyReason::ParentInvalid
        }
        _ => DenyReason::SnapshotInvalid,
    }
}

fn nonblocking(fd: i32) -> Result<()> {
    // SAFETY: F_GETFL reads flags of the owned worker pipe.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(Error::Io(
            "worker pipe flags",
            std::io::Error::last_os_error().raw_os_error(),
        ));
    }
    // SAFETY: F_SETFL only adds O_NONBLOCK to the owned worker pipe.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(Error::Io(
            "worker pipe flags",
            std::io::Error::last_os_error().raw_os_error(),
        ));
    }
    Ok(())
}

fn collect(child: &mut Child, deadline: Instant, bytes: &mut Vec<u8>) -> Result<()> {
    let stdout = child
        .stdout
        .as_ref()
        .ok_or(Error::EventInvalid("worker stdout"))?;
    nonblocking(stdout.as_raw_fd())?;
    let mut ended = false;
    loop {
        let mut chunk = [0_u8; 512];
        let read = child
            .stdout
            .as_mut()
            .ok_or(Error::EventInvalid("worker stdout"))?
            .read(&mut chunk);
        match read {
            Ok(0) => ended = true,
            Ok(count) if count <= RESPONSE_BYTES.saturating_sub(bytes.len()) => {
                bytes.extend_from_slice(&chunk[..count]);
            }
            Ok(_) => return Err(Error::EventInvalid("worker response size")),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(crate::error::io("worker response", &error)),
        }
        if ended
            && child
                .try_wait()
                .map_err(|error| crate::error::io("worker wait", &error))?
                .is_some()
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(Error::EventInvalid("worker timeout"));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn terminate(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn send_input(child: &mut Child, bytes: &[u8], deadline: Instant) -> Result<()> {
    let mut stdin = child
        .stdin
        .take()
        .ok_or(Error::EventInvalid("worker stdin"))?;
    nonblocking(stdin.as_raw_fd())?;
    let mut remaining = bytes;
    while !remaining.is_empty() {
        if Instant::now() >= deadline {
            return Err(Error::EventInvalid("worker input timeout"));
        }
        match stdin.write(remaining) {
            Ok(0) => return Err(Error::EventInvalid("worker stdin closed")),
            Ok(count) => remaining = &remaining[count..],
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) =>
            {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => return Err(crate::error::io("worker stdin", &error)),
        }
    }
    Ok(())
}

fn response(bytes: &[u8]) -> Result<ParsedResponse> {
    let lines = bytes.split(|byte| *byte == b'\n').collect::<Vec<_>>();
    if lines.last() != Some(&&[][..]) || !(2..=3).contains(&lines.len()) {
        return Err(Error::EventInvalid("worker framing"));
    }
    let messages = lines[..lines.len() - 1]
        .iter()
        .map(|line| serde_json::from_slice::<WorkerMessage>(line))
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|_| Error::EventInvalid("worker JSON"))?;
    match messages.as_slice() {
        [WorkerMessage::Denied { reason }] => Ok((None, *reason, false)),
        [
            WorkerMessage::Claimed {
                generation,
                attempt,
            },
            WorkerMessage::Neutral,
        ] => Ok((
            Some((*generation, *attempt)),
            DenyReason::SnapshotInvalid,
            true,
        )),
        [
            WorkerMessage::Claimed {
                generation,
                attempt,
            },
            WorkerMessage::Denied { reason },
        ] => Ok((Some((*generation, *attempt)), *reason, false)),
        _ => Err(Error::EventInvalid("worker sequence")),
    }
}

fn claimed_prefix(bytes: &[u8]) -> Option<(Uuid, Uuid)> {
    let end = bytes.iter().position(|byte| *byte == b'\n')?;
    match serde_json::from_slice::<WorkerMessage>(&bytes[..end]).ok()? {
        WorkerMessage::Claimed {
            generation,
            attempt,
        } => Some((generation, attempt)),
        _ => None,
    }
}

fn audit_denial(
    installation: &Installation,
    event: Option<&PreToolEvent>,
    reason: DenyReason,
) -> DenyReason {
    let gate = installation
        .active()
        .ok()
        .and_then(|active| active.config.gate());
    if let Some(gate) = gate
        && audit::append_preclaim(installation, event, reason.code(), gate).is_err()
    {
        return DenyReason::AuditFailed;
    }
    reason
}

fn reject_observed(installation: &Installation, event: &PreToolEvent, generation: Uuid) {
    if let Some(child) = event.child() {
        let _ = installation.reject_observed_child_generation(child, generation);
    } else {
        let _ = installation.reject_observed_generation(event.session_id(), generation);
    }
}

fn fail_attempt(
    installation: &Installation,
    event: &PreToolEvent,
    generation: Uuid,
    attempt: Uuid,
) {
    if let Some(child) = event.child() {
        let _ = installation.fail_child_attempt(child, generation, attempt);
    } else {
        let _ = installation.fail_attempt(event.session_id(), generation, attempt);
    }
}

fn ambiguous_root_denial(
    installation: &Installation,
    event: &PreToolEvent,
) -> Option<(DenyReason, bool)> {
    if event.child().is_none()
        && !installation
            .is_ambiguous(event.session_id())
            .is_ok_and(|ambiguous| !ambiguous)
    {
        return Some((
            audit_denial(installation, Some(event), DenyReason::UnsupportedCapability),
            false,
        ));
    }
    None
}

fn parse_supervised_event(
    installation: &Installation,
    client: ClientAdapter,
    bytes: &[u8],
) -> std::result::Result<PreToolEvent, (DenyReason, bool)> {
    match parse_event(client, bytes) {
        Ok(event) => Ok(event),
        Err(error) => {
            match identifiable_identity(bytes) {
                Some(IdentifiableIdentity::Root(session)) => {
                    let _ = installation.reject_event_session(&session);
                }
                Some(IdentifiableIdentity::Child(child)) => {
                    let _ = installation.reject_event_child(&child);
                }
                None => {}
            }
            Err((audit_denial(installation, None, map_error(error)), false))
        }
    }
}

fn spawn_worker(path: &Path, id: Uuid, client: ClientAdapter) -> Result<Child> {
    let executable =
        std::env::current_exe().map_err(|error| crate::error::io("worker executable", &error))?;
    let worker_key = Uuid::new_v4();
    Command::new(executable)
        .arg("pre-tool-worker")
        .arg("--installation")
        .arg(path)
        .arg("--installation-id")
        .arg(id.to_string())
        .arg("--client")
        .arg(client.name())
        .arg("--worker-key")
        .arg(worker_key.to_string())
        .env("MEMORY_HOOKS_WORKER_KEY", worker_key.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| crate::error::io("worker spawn", &error))
}

fn supervise() -> (DenyReason, bool) {
    let deadline = Instant::now() + DEADLINE;
    let (path, id, client) = match arguments() {
        Ok(value) => value,
        Err(error) => return (map_error(error), false),
    };
    let installation = match Installation::open(&path, id, client) {
        Ok(value) => value,
        Err(error) => return (map_error(error), false),
    };
    let event_bytes = match read_event_bytes(deadline) {
        Ok(bytes) => bytes,
        Err(error) => return (audit_denial(&installation, None, map_error(error)), false),
    };
    supervise_event(&installation, &event_bytes, deadline, || {
        spawn_worker(&path, id, client)
    })
}

fn supervise_event(
    installation: &Installation,
    event_bytes: &[u8],
    deadline: Instant,
    spawn: impl FnOnce() -> Result<Child>,
) -> (DenyReason, bool) {
    let client = installation.client();
    let event = match parse_supervised_event(installation, client, event_bytes) {
        Ok(value) => value,
        Err(denial) => return denial,
    };
    if let Some(denial) = ambiguous_root_denial(installation, &event) {
        return denial;
    }
    let observed = (if let Some(child) = event.child() {
        installation.read_child_snapshot(child, event.cwd())
    } else {
        installation.read_snapshot(event.session_id(), event.cwd())
    })
    .ok()
    .map(|snapshot| snapshot.generation);
    let Ok(mut child) = spawn() else {
        if let Some(generation) = observed {
            reject_observed(installation, &event, generation);
        }
        return (
            audit_denial(
                installation,
                Some(&event),
                DenyReason::UnsupportedCapability,
            ),
            false,
        );
    };
    let write_result = send_input(&mut child, event_bytes, deadline);
    if write_result.is_err() {
        terminate(&mut child);
        if let Some(generation) = observed {
            reject_observed(installation, &event, generation);
        }
        return (
            audit_denial(
                installation,
                Some(&event),
                DenyReason::UnsupportedCapability,
            ),
            false,
        );
    }
    let mut bytes = Vec::new();
    let collected = collect(&mut child, deadline, &mut bytes);
    if collected.is_err() {
        terminate(&mut child);
        if let Some((generation, attempt)) = claimed_prefix(&bytes) {
            fail_attempt(installation, &event, generation, attempt);
        } else if let Some(generation) = observed {
            reject_observed(installation, &event, generation);
        }
    }
    if collected.is_err() {
        return (
            audit_denial(installation, Some(&event), DenyReason::SnapshotInvalid),
            false,
        );
    }
    let status = child.try_wait().ok().flatten();
    let parsed = response(&bytes);
    let claim = parsed
        .as_ref()
        .ok()
        .and_then(|(claim, _, _)| *claim)
        .or_else(|| claimed_prefix(&bytes));
    if let Some((generation, attempt)) = claim
        && (status.is_none_or(|status| !status.success())
            || !parsed.as_ref().is_ok_and(|(_, _, neutral)| *neutral))
    {
        fail_attempt(installation, &event, generation, attempt);
    } else if (status.is_none_or(|status| !status.success())
        || parsed.is_err()
        || parsed.as_ref().is_ok_and(|(claim, reason, _)| {
            claim.is_none() && !matches!(reason, DenyReason::UnsupportedCapability)
        }))
        && let Some(generation) = observed
    {
        reject_observed(installation, &event, generation);
    }
    match (status, parsed) {
        (Some(status), Ok((_, reason, true))) if status.success() => (reason, true),
        (Some(status), Ok((_, reason, false))) if status.success() => (reason, false),
        _ => (
            audit_denial(installation, Some(&event), DenyReason::SnapshotInvalid),
            false,
        ),
    }
}

// Test binaries can exercise the exact bounded supervisor with a dedicated
// protocol pipe. Production always spawns the protected current executable.
#[cfg(test)]
pub(crate) fn fixture_supervise(
    installation: &Installation,
    event_bytes: &[u8],
    deadline: Instant,
    child: Child,
) -> (DenyReason, bool) {
    supervise_event(installation, event_bytes, deadline, || Ok(child))
}

/// Run the protected outer command. Every failure returns a blocking result.
#[must_use]
pub fn run_supervisor() -> i32 {
    publish_decision(supervise(), || {
        DeadlineWriter::new(Instant::now() + Duration::from_secs(2))
            .map_err(|error| crate::error::io("denial output", &error))
    })
}

fn publish_decision<W: Write>(
    (reason, neutral): (DenyReason, bool),
    writer: impl FnOnce() -> Result<W>,
) -> i32 {
    if neutral {
        return 0;
    }
    let bytes = deny_json(reason.code());
    let write = bytes.and_then(|bytes| {
        let mut writer = writer()?;
        writer
            .write_all(&bytes)
            .map_err(|error| crate::error::io("denial output", &error))?;
        writer
            .flush()
            .map_err(|error| crate::error::io("denial output", &error))
    });
    if write.is_ok() {
        0
    } else {
        eprintln!("pre_tool: deny output unavailable");
        2
    }
}

#[cfg(test)]
pub(crate) fn fixture_decision(result: (DenyReason, bool), writer: &mut Vec<u8>) -> i32 {
    publish_decision(result, || Ok(writer))
}

/// Run only the internal worker protocol; the outer process validates it.
#[must_use]
pub fn run_worker_entry() -> i32 {
    let Ok((path, id, client)) = arguments() else {
        return 2;
    };
    let Ok(installation) = Installation::open(&path, id, client) else {
        return 2;
    };
    let mut stdout = std::io::stdout().lock();
    match run_worker(&installation, &mut stdout) {
        Ok(()) => 0,
        Err(error) => {
            let message = WorkerMessage::Denied {
                reason: map_error(error),
            };
            let bytes = serde_json::to_vec(&message);
            let Ok(mut bytes) = bytes else {
                return 2;
            };
            bytes.push(b'\n');
            if stdout.write_all(&bytes).is_err() || stdout.flush().is_err() {
                2
            } else {
                0
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::{claimed_prefix, response};

    #[test]
    fn partial_or_unexpected_worker_output_never_means_neutral() {
        let generation = Uuid::new_v4();
        let attempt = Uuid::new_v4();
        let claim = format!(
            "{{\"kind\":\"claimed\",\"generation\":\"{generation}\",\"attempt\":\"{attempt}\"}}\n"
        );
        assert_eq!(
            claimed_prefix(claim.as_bytes()),
            Some((generation, attempt))
        );
        assert!(response(claim.as_bytes()).is_err());
        assert!(response(format!("{claim}garbage\n").as_bytes()).is_err());
        assert!(response(b"\n").is_err());
        assert!(response(b"{\"kind\":\"neutral\"}\n").is_err());
        assert!(response(format!("{claim}{{\"kind\":\"neutral\"}}\n").as_bytes()).is_ok());
    }
}
