//! Bounded Codex and Claude pre-tool event contracts and neutral/deny output.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use memory_common::error::Error as SharedError;
use memory_common::http_client::HttpMemoryClient;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::audit;
use crate::config::ClientAdapter;
use crate::error::{Error, Result};
use crate::installation::Installation;
use crate::limits;
use crate::session_start::read_event_bytes;

/// Maximum UTF-8 bytes in a single client event.
pub const EVENT_BYTES: usize = 64 * 1024;

/// Safe category for local audit records. Unknown names remain guarded.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCategory {
    /// A shell or command execution tool.
    Shell,
    /// An edit, write, or patch tool.
    Write,
    /// A model context protocol tool.
    Mcp,
    /// A parent agent or spawn tool.
    Agent,
    /// Every other tool name.
    Unknown,
}

/// Validated event identity, without retaining tool arguments or freeform metadata.
pub struct PreToolEvent {
    session_id: String,
    cwd: PathBuf,
    category: ToolCategory,
}

impl PreToolEvent {
    /// Bounded client session identifier. It must only be used for keyed hashing.
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Canonical process working directory, checked against client input.
    #[must_use]
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Exact adapter classification used for redacted audit.
    #[must_use]
    pub const fn category(&self) -> ToolCategory {
        self.category
    }
}

#[derive(Deserialize)]
struct CodexEvent {
    session_id: String,
    cwd: String,
    hook_event_name: String,
    tool_name: String,
    tool_use_id: String,
    tool_input: Value,
    turn_id: String,
    #[serde(default, deserialize_with = "present")]
    parent_session_id: Presence,
    #[serde(default, deserialize_with = "present")]
    subagent_id: Presence,
    #[serde(default, deserialize_with = "present")]
    is_subagent: Presence,
    #[serde(default, deserialize_with = "present")]
    agent_id: Presence,
}

#[derive(Deserialize)]
struct ClaudeEvent {
    session_id: String,
    cwd: String,
    hook_event_name: String,
    tool_name: String,
    tool_use_id: String,
    tool_input: Value,
    #[serde(default, deserialize_with = "present")]
    turn_id: Presence,
    #[serde(default, deserialize_with = "present")]
    parent_session_id: Presence,
    #[serde(default, deserialize_with = "present")]
    subagent_id: Presence,
    #[serde(default, deserialize_with = "present")]
    is_subagent: Presence,
    #[serde(default, deserialize_with = "present")]
    agent_id: Presence,
}

#[derive(Deserialize)]
struct SessionProbe {
    session_id: Option<String>,
    #[serde(default, deserialize_with = "present")]
    parent_session_id: Presence,
    #[serde(default, deserialize_with = "present")]
    subagent_id: Presence,
    #[serde(default, deserialize_with = "present")]
    is_subagent: Presence,
    #[serde(default, deserialize_with = "present")]
    agent_id: Presence,
}

/// Return only an unambiguous bounded session identity from malformed input.
pub(crate) fn identifiable_session(bytes: &[u8]) -> Option<String> {
    if bytes.len() > EVENT_BYTES {
        return None;
    }
    let probe: SessionProbe = serde_json::from_slice(bytes).ok()?;
    if [
        probe.parent_session_id,
        probe.subagent_id,
        probe.is_subagent,
        probe.agent_id,
    ]
    .contains(&Presence::Present)
    {
        return None;
    }
    let session = probe.session_id?;
    nonempty_bounded(&session, limits::EXTERNAL_ID_BYTES).then_some(session)
}

#[derive(Clone, Copy, Default, Eq, PartialEq)]
enum Presence {
    #[default]
    Missing,
    Present,
}

fn present<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Presence, D::Error> {
    let _ = Value::deserialize(deserializer)?;
    Ok(Presence::Present)
}

fn nonempty_bounded(value: &str, limit: usize) -> bool {
    !value.is_empty() && value.len() <= limit && !value.chars().any(char::is_control)
}

fn checked_path(raw: &str) -> Result<PathBuf> {
    if !nonempty_bounded(raw, limits::PATH_BYTES) || !Path::new(raw).is_absolute() {
        return Err(Error::EventInvalid("cwd"));
    }
    std::fs::canonicalize(raw).map_err(|_| Error::EventInvalid("cwd"))
}

fn category(adapter: ClientAdapter, tool: &str) -> ToolCategory {
    match (adapter, tool) {
        (_, "Bash") => ToolCategory::Shell,
        (ClientAdapter::CodexV1, "apply_patch")
        | (ClientAdapter::ClaudeV1, "Edit" | "Write" | "MultiEdit" | "NotebookEdit") => {
            ToolCategory::Write
        }
        (_, "Task" | "Agent" | "TaskCreate" | "TaskUpdate") => ToolCategory::Agent,
        _ => ToolCategory::Unknown,
    }
}

#[derive(Clone, Copy)]
struct EventParts<'a> {
    session_id: &'a str,
    cwd: &'a str,
    event: &'a str,
    tool_name: &'a str,
    tool_use_id: &'a str,
    input: &'a Value,
    delegated: bool,
    client_shape_valid: bool,
}

fn checked(adapter: ClientAdapter, parts: EventParts<'_>) -> Result<PreToolEvent> {
    if !nonempty_bounded(parts.session_id, limits::EXTERNAL_ID_BYTES)
        || !nonempty_bounded(parts.tool_use_id, limits::EXTERNAL_ID_BYTES)
        || !nonempty_bounded(parts.tool_name, limits::EXTERNAL_ID_BYTES)
        || parts.event != "PreToolUse"
        || parts.delegated
        || !parts.client_shape_valid
    {
        return Err(Error::EventInvalid("pre-tool identity"));
    }
    let input = parts
        .input
        .as_object()
        .ok_or(Error::EventInvalid("tool_input"))?;
    if matches!(parts.tool_name, "Bash" | "apply_patch")
        && input
            .get("command")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
    {
        return Err(Error::EventInvalid("tool command"));
    }
    if adapter == ClientAdapter::ClaudeV1
        && match parts.tool_name {
            "Edit" => {
                !input.get("file_path").is_some_and(Value::is_string)
                    || !input.get("old_string").is_some_and(Value::is_string)
                    || !input.get("new_string").is_some_and(Value::is_string)
            }
            "Write" => {
                !input.get("file_path").is_some_and(Value::is_string)
                    || !input.get("content").is_some_and(Value::is_string)
            }
            _ => false,
        }
    {
        return Err(Error::EventInvalid("tool_input"));
    }
    let cwd = checked_path(parts.cwd)?;
    let actual = std::env::current_dir()
        .and_then(std::fs::canonicalize)
        .map_err(|_| Error::EventInvalid("process cwd"))?;
    if cwd != actual {
        return Err(Error::EventInvalid("cwd mismatch"));
    }
    let explicit = [input.get("cwd"), input.get("workdir")]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    if parts.tool_name == "Bash" && explicit.len() > 1 {
        return Err(Error::EventInvalid("ambiguous tool cwd"));
    }
    if parts.tool_name == "Bash"
        && let Some(location) = explicit.first()
    {
        let location = location.as_str().ok_or(Error::EventInvalid("tool cwd"))?;
        if checked_path(location)? != actual {
            return Err(Error::EventInvalid("tool cwd mismatch"));
        }
    }
    Ok(PreToolEvent {
        session_id: parts.session_id.to_owned(),
        cwd,
        category: category(adapter, parts.tool_name),
    })
}

/// Parse one complete client event without treating untrusted metadata as authority.
///
/// # Errors
/// Rejects malformed, oversized, delegated, ambiguous, or wrong-cwd input.
pub fn parse_event(adapter: ClientAdapter, bytes: &[u8]) -> Result<PreToolEvent> {
    if bytes.len() > EVENT_BYTES {
        return Err(Error::EventTooLarge);
    }
    let source = std::str::from_utf8(bytes).map_err(|_| Error::EventInvalid("UTF-8"))?;
    match adapter {
        ClientAdapter::CodexV1 => {
            let event: CodexEvent =
                serde_json::from_str(source).map_err(|_| Error::EventInvalid("Codex JSON"))?;
            checked(
                adapter,
                EventParts {
                    session_id: &event.session_id,
                    cwd: &event.cwd,
                    event: &event.hook_event_name,
                    tool_name: &event.tool_name,
                    tool_use_id: &event.tool_use_id,
                    input: &event.tool_input,
                    delegated: [
                        event.parent_session_id,
                        event.subagent_id,
                        event.is_subagent,
                        event.agent_id,
                    ]
                    .contains(&Presence::Present),
                    client_shape_valid: nonempty_bounded(&event.turn_id, limits::EXTERNAL_ID_BYTES),
                },
            )
        }
        ClientAdapter::ClaudeV1 => {
            let event: ClaudeEvent =
                serde_json::from_str(source).map_err(|_| Error::EventInvalid("Claude JSON"))?;
            checked(
                adapter,
                EventParts {
                    session_id: &event.session_id,
                    cwd: &event.cwd,
                    event: &event.hook_event_name,
                    tool_name: &event.tool_name,
                    tool_use_id: &event.tool_use_id,
                    input: &event.tool_input,
                    delegated: [
                        event.parent_session_id,
                        event.subagent_id,
                        event.is_subagent,
                        event.agent_id,
                    ]
                    .contains(&Presence::Present),
                    client_shape_valid: event.turn_id == Presence::Missing,
                },
            )
        }
    }
}

#[derive(Serialize)]
struct Deny<'a> {
    #[serde(rename = "hookSpecificOutput")]
    output: DenyOutput<'a>,
}

#[derive(Serialize)]
struct DenyOutput<'a> {
    #[serde(rename = "hookEventName")]
    event: &'static str,
    #[serde(rename = "permissionDecision")]
    decision: &'static str,
    #[serde(rename = "permissionDecisionReason")]
    reason: &'a str,
}

/// Serialize a client deny decision with a fixed, allowlisted reason code.
///
/// # Errors
/// Rejects an unknown reason or serialization failure.
pub fn deny_json(reason: &'static str) -> Result<Vec<u8>> {
    if !matches!(
        reason,
        "fresh_session_required"
            | "policy_changed"
            | "guardrails_unavailable"
            | "snapshot_invalid"
            | "scope_mismatch"
            | "unsupported_capability"
            | "audit_failed"
            | "event_invalid"
            | "gate_contract_required"
    ) {
        return Err(Error::EventInvalid("denial reason"));
    }
    let mut bytes = serde_json::to_vec(&Deny {
        output: DenyOutput {
            event: "PreToolUse",
            decision: "deny",
            reason,
        },
    })
    .map_err(|_| Error::EventInvalid("denial JSON"))?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// Complete, bounded worker protocol. Neutral requires the final variant.
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum WorkerMessage {
    Claimed { generation: Uuid, attempt: Uuid },
    Neutral,
    Denied { reason: DenyReason },
}

/// Fixed denial reasons that cannot contain event or daemon text.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DenyReason {
    FreshSessionRequired,
    PolicyChanged,
    GuardrailsUnavailable,
    SnapshotInvalid,
    ScopeMismatch,
    UnsupportedCapability,
    AuditFailed,
    EventInvalid,
    GateContractRequired,
}

impl DenyReason {
    pub(crate) const fn code(self) -> &'static str {
        match self {
            Self::FreshSessionRequired => "fresh_session_required",
            Self::PolicyChanged => "policy_changed",
            Self::GuardrailsUnavailable => "guardrails_unavailable",
            Self::SnapshotInvalid => "snapshot_invalid",
            Self::ScopeMismatch => "scope_mismatch",
            Self::UnsupportedCapability => "unsupported_capability",
            Self::AuditFailed => "audit_failed",
            Self::EventInvalid => "event_invalid",
            Self::GateContractRequired => "gate_contract_required",
        }
    }
}

fn write_message(writer: &mut impl Write, message: &WorkerMessage) -> Result<()> {
    let mut bytes = serde_json::to_vec(message).map_err(|_| Error::EventInvalid("worker JSON"))?;
    if bytes.len() > 512 {
        return Err(Error::EventInvalid("worker size"));
    }
    bytes.push(b'\n');
    writer
        .write_all(&bytes)
        .map_err(|error| crate::error::io("worker write", &error))?;
    writer
        .flush()
        .map_err(|error| crate::error::io("worker flush", &error))
}

fn fresh_pack(
    claim: &crate::snapshot::ClaimedCheck,
) -> std::result::Result<memory_common::guardrails::GuardrailPack, DenyReason> {
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .map_err(|_| DenyReason::GuardrailsUnavailable)?;
    let client = HttpMemoryClient::with_http_client(&claim.origin, claim.api_token.clone(), http)
        .map_err(|_| DenyReason::GuardrailsUnavailable)?
        .with_context(claim.context.clone());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| DenyReason::GuardrailsUnavailable)?;
    runtime
        .block_on(client.guardrails(&claim.project))
        .map_err(|error| match error {
            SharedError::Policy { code, .. } if code == "guardrails_scope_mismatch" => {
                DenyReason::ScopeMismatch
            }
            _ => DenyReason::GuardrailsUnavailable,
        })
}

/// Execute one check inside a supervised worker; this output is internal only.
///
/// # Errors
/// The supervisor treats any error, incomplete output, or worker crash as deny.
pub(crate) fn run_worker(installation: &Installation, writer: &mut impl Write) -> Result<()> {
    let bytes = read_event_bytes(Instant::now() + Duration::from_secs(60))?;
    let event = parse_event(installation.client(), &bytes)?;
    let active = installation.active()?;
    let gate = active
        .config
        .gate()
        .ok_or(Error::ConfigInvalid("gate_contract_required"))?;
    let claim = match installation.claim_check(event.session_id(), event.cwd()) {
        Ok(claim) => claim,
        Err(error) => {
            if audit::append_preclaim(installation, Some(&event), "snapshot_invalid", gate).is_err()
            {
                return write_message(
                    writer,
                    &WorkerMessage::Denied {
                        reason: DenyReason::AuditFailed,
                    },
                );
            }
            return Err(error);
        }
    };
    if let Err(error) = write_message(
        writer,
        &WorkerMessage::Claimed {
            generation: claim.generation,
            attempt: claim.attempt,
        },
    ) {
        let _ = installation.fail_check(&claim);
        return Err(error);
    }
    let fetched = fresh_pack(&claim);
    let reason = match fetched {
        Err(reason) => Some(reason),
        Ok(pack) if pack.validate_for(&claim.project, &claim.context).is_err() => {
            Some(DenyReason::ScopeMismatch)
        }
        Ok(pack) if pack != claim.pack => Some(DenyReason::PolicyChanged),
        Ok(_) => None,
    };
    if let Some(reason) = reason {
        let audit_result = audit::append(
            installation,
            &claim,
            installation.client(),
            event.category(),
            "deny",
            reason.code(),
            gate,
        );
        installation.fail_check(&claim)?;
        let reason = if audit_result.is_err() {
            DenyReason::AuditFailed
        } else {
            reason
        };
        return write_message(writer, &WorkerMessage::Denied { reason });
    }
    if let Err(error) = installation.finish_check(&claim, || {
        audit::append(
            installation,
            &claim,
            installation.client(),
            event.category(),
            "neutral",
            "checked",
            gate,
        )
    }) {
        installation.fail_check(&claim)?;
        return write_message(
            writer,
            &WorkerMessage::Denied {
                reason: if error == Error::AuditFailed {
                    DenyReason::AuditFailed
                } else {
                    DenyReason::SnapshotInvalid
                },
            },
        );
    }
    if let Err(error) = write_message(writer, &WorkerMessage::Neutral) {
        let _ = installation.fail_check(&claim);
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{
        ClientAdapter, EVENT_BYTES, ToolCategory, deny_json, identifiable_session, parse_event,
    };

    fn event(tool: &str, input: &Value) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "session_id": "session",
            "cwd": std::env::current_dir().unwrap(),
            "hook_event_name": "PreToolUse",
            "tool_name": tool,
            "tool_use_id": "call",
            "tool_input": input,
            "turn_id": "turn",
        }))
        .unwrap()
    }

    #[test]
    fn every_category_stays_guarded_without_a_command() {
        for adapter in [ClientAdapter::CodexV1, ClientAdapter::ClaudeV1] {
            let cases = [
                ("Bash", ToolCategory::Shell, json!({"command":"true"})),
                ("mcp__server__write", ToolCategory::Unknown, json!({})),
                ("Agent", ToolCategory::Agent, json!({})),
                ("unrecognized", ToolCategory::Unknown, json!({})),
            ];
            for (name, expected, input) in cases {
                let mut value: Value = serde_json::from_slice(&event(name, &input)).unwrap();
                if adapter == ClientAdapter::ClaudeV1 {
                    value.as_object_mut().unwrap().remove("turn_id");
                }
                let parsed = parse_event(adapter, &serde_json::to_vec(&value).unwrap()).unwrap();
                assert_eq!(parsed.category(), expected);
            }
        }
        assert_eq!(
            parse_event(
                ClientAdapter::CodexV1,
                &event("apply_patch", &json!({"command":"secret"}))
            )
            .unwrap()
            .category(),
            ToolCategory::Write
        );
        let mut edit: Value = serde_json::from_slice(&event(
            "Edit",
            &json!({"file_path":"/a", "old_string":"old", "new_string":"secret"}),
        ))
        .unwrap();
        edit.as_object_mut().unwrap().remove("turn_id");
        assert_eq!(
            parse_event(ClientAdapter::ClaudeV1, &serde_json::to_vec(&edit).unwrap())
                .unwrap()
                .category(),
            ToolCategory::Write
        );
    }

    #[test]
    fn malformed_and_ambiguous_events_are_rejected() {
        let adapter = ClientAdapter::CodexV1;
        let duplicate = br#"{"session_id":"a","session_id":"b"}"#;
        assert!(parse_event(adapter, duplicate).is_err());
        assert!(parse_event(adapter, &[0xff]).is_err());
        let mut trailing = event("Bash", &json!({"command":"true"}));
        trailing.extend_from_slice(b"{}");
        assert!(parse_event(adapter, &trailing).is_err());
        let mut delegated: Value =
            serde_json::from_slice(&event("Bash", &json!({"command":"true"}))).unwrap();
        delegated["is_subagent"] = Value::Null;
        assert!(parse_event(adapter, &serde_json::to_vec(&delegated).unwrap()).is_err());
        let input = json!({"command":"true", "cwd": std::env::current_dir().unwrap(), "workdir": std::env::current_dir().unwrap()});
        assert!(parse_event(adapter, &event("Bash", &input)).is_err());
        assert!(
            parse_event(
                adapter,
                &event("Bash", &json!({"command":"true", "workdir":"/"}))
            )
            .is_err()
        );
        let mut duplicate = event("Bash", &json!({"command":"true"}));
        duplicate.pop();
        duplicate.extend_from_slice(b",\"tool_use_id\":\"second\"}");
        assert!(parse_event(adapter, &duplicate).is_err());
        let mut wrong_event: Value =
            serde_json::from_slice(&event("Bash", &json!({"command":"true"}))).unwrap();
        wrong_event["hook_event_name"] = json!("PostToolUse");
        assert!(parse_event(adapter, &serde_json::to_vec(&wrong_event).unwrap()).is_err());
        let mut bounded = event("Bash", &json!({"command":"true"}));
        bounded.resize(EVENT_BYTES, b' ');
        assert!(parse_event(adapter, &bounded).is_ok());
        bounded.push(b' ');
        assert!(parse_event(adapter, &bounded).is_err());
        let mut claude: Value =
            serde_json::from_slice(&event("Bash", &json!({"command":"true"}))).unwrap();
        assert!(
            parse_event(
                ClientAdapter::ClaudeV1,
                &serde_json::to_vec(&claude).unwrap()
            )
            .is_err()
        );
        claude.as_object_mut().unwrap().remove("turn_id");
        assert!(
            parse_event(
                ClientAdapter::ClaudeV1,
                &serde_json::to_vec(&claude).unwrap()
            )
            .is_ok()
        );
    }

    #[test]
    fn agent_identity_cannot_fall_back_to_root_on_parse_failure() {
        for adapter in [ClientAdapter::CodexV1, ClientAdapter::ClaudeV1] {
            let mut value: Value = serde_json::from_slice(&event("Agent", &json!({}))).unwrap();
            if adapter == ClientAdapter::ClaudeV1 {
                value.as_object_mut().unwrap().remove("turn_id");
            }
            for field in [
                "agent_id",
                "parent_session_id",
                "subagent_id",
                "is_subagent",
            ] {
                value[field] = Value::Null;
                let bytes = serde_json::to_vec(&value).unwrap();
                assert!(parse_event(adapter, &bytes).is_err());
                assert_eq!(identifiable_session(&bytes), None);
                value.as_object_mut().unwrap().remove(field);
            }
            value["agent_id"] = json!("child");
            let bytes = serde_json::to_vec(&value).unwrap();
            assert!(parse_event(adapter, &bytes).is_err());
            assert_eq!(identifiable_session(&bytes), None);
            value.as_object_mut().unwrap().remove("agent_id");
            assert_eq!(
                identifiable_session(&serde_json::to_vec(&value).unwrap()),
                Some("session".to_owned())
            );
        }
    }

    #[test]
    fn deny_contract_cannot_grant_permission() {
        let value: Value = serde_json::from_slice(&deny_json("policy_changed").unwrap()).unwrap();
        assert_eq!(value["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        assert_eq!(value["hookSpecificOutput"]["permissionDecision"], "deny");
        assert!(value["hookSpecificOutput"].get("updatedInput").is_none());
        assert!(deny_json("allow").is_err());
    }
}
