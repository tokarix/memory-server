//! Bounded client event parsing and fresh mandatory `SessionStart` delivery.

use std::io::{self, Write};
use std::os::fd::RawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use memory_common::error::Error as SharedError;
use memory_common::http_client::HttpMemoryClient;
use serde::Deserialize;
use serde_json::Value;

use crate::config::ClientAdapter;
use crate::error::{Error, Result};
use crate::installation::Installation;
use crate::limits;
use crate::snapshot::PendingGeneration;

/// Maximum accepted client event bytes, including JSON framing.
pub const EVENT_BYTES: usize = 64 * 1024;
const HELPER_DEADLINE: Duration = Duration::from_secs(60);

#[derive(Deserialize)]
struct Event {
    session_id: Option<Value>,
    cwd: Option<Value>,
    hook_event_name: Option<Value>,
    source: Option<Value>,
    parent_session_id: Option<Value>,
    subagent_id: Option<Value>,
    is_subagent: Option<Value>,
}

fn string_field<'a>(field: Option<&'a Value>, name: &'static str) -> Result<&'a str> {
    field
        .and_then(Value::as_str)
        .ok_or(Error::EventInvalid(name))
}

fn checked_event(event: &Event, adapter: ClientAdapter) -> Result<PathBuf> {
    if string_field(event.hook_event_name.as_ref(), "hook_event_name")? != "SessionStart" {
        return Err(Error::EventInvalid("hook_event_name"));
    }
    let source = string_field(event.source.as_ref(), "source")?;
    if !(matches!(source, "startup" | "resume" | "clear" | "compact")
        || adapter == ClientAdapter::ClaudeV1 && source == "fork")
    {
        return Err(Error::EventInvalid("source"));
    }
    if event.parent_session_id.is_some()
        || event.subagent_id.is_some()
        || event.is_subagent.is_some()
    {
        return Err(Error::EventInvalid("delegation"));
    }
    let cwd = string_field(event.cwd.as_ref(), "cwd")?;
    if cwd.is_empty() || cwd.len() > limits::PATH_BYTES || !Path::new(cwd).is_absolute() {
        return Err(Error::EventInvalid("cwd"));
    }
    let supplied = std::fs::canonicalize(cwd).map_err(|_| Error::EventInvalid("cwd"))?;
    let actual = std::env::current_dir()
        .and_then(std::fs::canonicalize)
        .map_err(|_| Error::EventInvalid("process cwd"))?;
    if supplied != actual {
        return Err(Error::EventInvalid("cwd mismatch"));
    }
    Ok(supplied)
}

fn deadline_poll(fd: RawFd, events: i16, deadline: Instant) -> io::Result<()> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "deadline"));
        }
        let millis = i32::try_from(remaining.as_millis().min(i32::MAX as u128))
            .map_err(|_| io::ErrorKind::InvalidInput)?;
        let mut poll_fd = libc::pollfd {
            fd,
            events,
            revents: 0,
        };
        // SAFETY: poll_fd is a valid initialized single-entry poll array.
        let result = unsafe { libc::poll(&raw mut poll_fd, 1, millis) };
        if result > 0 {
            return Ok(());
        }
        if result == 0 {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "deadline"));
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: fcntl only inspects or changes flags on this process's open fd.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: F_SETFL takes the existing flags with O_NONBLOCK added.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn read_event(deadline: Instant) -> Result<Event> {
    nonblocking(libc::STDIN_FILENO).map_err(|error| crate::error::io("input flags", &error))?;
    let mut bytes = Vec::with_capacity(4096);
    loop {
        deadline_poll(libc::STDIN_FILENO, libc::POLLIN, deadline)
            .map_err(|error| crate::error::io("input deadline", &error))?;
        let mut chunk = [0_u8; 4096];
        // SAFETY: chunk is writable for its entire fixed capacity.
        let count =
            unsafe { libc::read(libc::STDIN_FILENO, chunk.as_mut_ptr().cast(), chunk.len()) };
        if count == 0 {
            break;
        }
        if count < 0 {
            let error = io::Error::last_os_error();
            if matches!(
                error.kind(),
                io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
            ) {
                continue;
            }
            return Err(crate::error::io("input read", &error));
        }
        let count = usize::try_from(count).map_err(|_| Error::EventInvalid("input count"))?;
        if bytes.len().saturating_add(count) > EVENT_BYTES {
            return Err(Error::EventTooLarge);
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
    serde_json::from_slice(&bytes).map_err(|_| Error::EventInvalid("JSON object"))
}

struct DeadlineWriter {
    deadline: Instant,
}

impl DeadlineWriter {
    fn new(deadline: Instant) -> io::Result<Self> {
        nonblocking(libc::STDOUT_FILENO)?;
        Ok(Self { deadline })
    }
}

impl Write for DeadlineWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        loop {
            deadline_poll(libc::STDOUT_FILENO, libc::POLLOUT, self.deadline)?;
            // SAFETY: bytes is a valid readable buffer for its entire length.
            let count =
                unsafe { libc::write(libc::STDOUT_FILENO, bytes.as_ptr().cast(), bytes.len()) };
            if count >= 0 {
                return usize::try_from(count).map_err(|_| io::ErrorKind::InvalidData.into());
            }
            let error = io::Error::last_os_error();
            if !matches!(
                error.kind(),
                io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
            ) {
                return Err(error);
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        if Instant::now() >= self.deadline {
            return Err(io::ErrorKind::TimedOut.into());
        }
        Ok(())
    }
}

fn map_fetch_error(error: &SharedError) -> Error {
    match error {
        SharedError::Policy { code, .. } => {
            let reason = match code.as_str() {
                "guardrails_empty" => "empty",
                "guardrails_conflict" => "conflict",
                "guardrails_malformed" => "malformed",
                "guardrails_too_large" => "too_large",
                "guardrails_upstream_unsupported" => "unsupported",
                _ => "policy",
            };
            Error::GuardrailsUnavailable(reason)
        }
        SharedError::Transport(_) => Error::GuardrailsUnavailable("transport"),
        _ => Error::GuardrailsUnavailable("upstream"),
    }
}

fn at_stage<T>(stage: &'static str, pending: &PendingGeneration, result: Result<T>) -> Result<T> {
    if let Err(error) = &result {
        eprintln!(
            "session_start: stage={stage} generation={} code={}",
            pending.generation(),
            error.code()
        );
    }
    result
}

fn safe_http_status(error: &SharedError) -> Option<u16> {
    match error {
        SharedError::Policy { details, .. } => details
            .get("http_status")
            .and_then(Value::as_u64)
            .and_then(|status| u16::try_from(status).ok())
            .filter(|status| (100..=599).contains(status)),
        _ => None,
    }
}

/// Consume one supported top-level event and emit its fresh exact publication.
///
/// # Errors
/// Errors leave the claimed session generation unusable. No old head is reused.
pub fn run(installation: &Installation) -> Result<()> {
    let deadline = Instant::now() + HELPER_DEADLINE;
    let event = read_event(deadline)?;
    let session = string_field(event.session_id.as_ref(), "session_id")?;
    if session.is_empty() || session.len() > limits::EXTERNAL_ID_BYTES {
        return Err(Error::EventInvalid("session_id length"));
    }
    let pending = installation.begin_session(session)?;
    let cwd = at_stage(
        "event",
        &pending,
        checked_event(&event, installation.client()),
    )?;
    let binding = at_stage(
        "binding",
        &pending,
        installation.attach_binding(&pending, &cwd),
    )?;
    let active = at_stage("configuration", &pending, installation.active())?;
    if active.epoch != pending.epoch() || active.config.delivery().is_none() {
        return at_stage("configuration", &pending, Err(Error::SessionStale));
    }
    let delivery = at_stage(
        "configuration",
        &pending,
        active.config.delivery().ok_or(Error::InstallationInactive),
    )?;
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .map_err(|_| Error::GuardrailsUnavailable("client setup"));
    let http = at_stage("transport", &pending, http)?;
    let client = at_stage(
        "transport",
        &pending,
        HttpMemoryClient::with_http_client(
            delivery.origin(),
            delivery.api_token().map(str::to_owned),
            http,
        )
        .map_err(|_| Error::GuardrailsUnavailable("client setup")),
    )?
    .with_context(binding.context().clone());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| Error::GuardrailsUnavailable("runtime"));
    let runtime = at_stage("runtime", &pending, runtime)?;
    let pack = runtime
        .block_on(client.guardrails(binding.project()))
        .map_err(|error| {
            let mapped = map_fetch_error(&error);
            let status =
                safe_http_status(&error).map_or("unknown".to_owned(), |value| value.to_string());
            eprintln!(
                "session_start: stage=fetch generation={} code={} http_status={status}",
                pending.generation(),
                mapped.code()
            );
            mapped
        })?;
    let writer =
        DeadlineWriter::new(deadline).map_err(|error| crate::error::io("output flags", &error));
    let mut writer = at_stage("output", &pending, writer)?;
    at_stage(
        "completion",
        &pending,
        installation.complete_session(&pending, &cwd, pack, &mut writer),
    )
}

#[cfg(test)]
mod tests {
    use super::{ClientAdapter, Event, checked_event};

    #[test]
    fn duplicate_and_wrong_type_fields_fail() {
        let duplicate = br#"{"session_id":"a","session_id":"b"}"#;
        assert!(serde_json::from_slice::<Event>(duplicate).is_err());
        let wrong: Event = serde_json::from_slice(
            br#"{"session_id":"a","cwd":1,"hook_event_name":"SessionStart","source":"startup"}"#,
        )
        .unwrap();
        assert!(checked_event(&wrong, ClientAdapter::CodexV1).is_err());
    }
}
