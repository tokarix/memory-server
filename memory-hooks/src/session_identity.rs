//! Typed delegated lifecycle identity at the client JSON boundary.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::config::{ClientAdapter, hash_part};
use crate::error::{Error, Result};
use crate::limits;

/// A child identity scoped to one enclosing client session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChildIdentity {
    session_id: String,
    agent_id: String,
}

impl ChildIdentity {
    /// The enclosing client session identifier.
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// The client's separate child identifier.
    #[must_use]
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    /// Domain-separated stable key for one child in a protected installation.
    #[must_use]
    pub fn store_hash(&self, installation_id: Uuid, adapter: ClientAdapter) -> String {
        let mut hash = Sha256::new();
        hash.update(b"memory-hooks-control-child-v1\0");
        hash_part(&mut hash, installation_id.as_bytes());
        hash_part(&mut hash, adapter.name().as_bytes());
        hash_part(&mut hash, self.session_id.as_bytes());
        hash_part(&mut hash, self.agent_id.as_bytes());
        format!("{:x}", hash.finalize())
    }
}

/// A validated child startup event; it is not publication evidence.
pub struct DelegatedStart {
    identity: ChildIdentity,
    cwd: PathBuf,
}

impl DelegatedStart {
    /// The child identity carried by the lifecycle event.
    #[must_use]
    pub const fn identity(&self) -> &ChildIdentity {
        &self.identity
    }

    /// Canonical process working directory, checked against client input.
    #[must_use]
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }
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

#[derive(Deserialize)]
struct CommonStart {
    session_id: String,
    agent_id: String,
    agent_type: String,
    cwd: String,
    hook_event_name: String,
    #[serde(default, deserialize_with = "present")]
    parent_session_id: Presence,
    #[serde(default, deserialize_with = "present")]
    subagent_id: Presence,
    #[serde(default, deserialize_with = "present")]
    is_subagent: Presence,
}

#[derive(Deserialize)]
struct CodexStart {
    #[serde(flatten)]
    common: CommonStart,
    turn_id: String,
}

#[derive(Deserialize)]
struct ClaudeStart {
    #[serde(flatten)]
    common: CommonStart,
    #[serde(default, deserialize_with = "present")]
    turn_id: Presence,
}

fn bounded(value: &str, limit: usize) -> bool {
    !value.is_empty() && value.len() <= limit && !value.chars().any(char::is_control)
}

fn checked(common: CommonStart, codex_turn: Option<&str>) -> Result<DelegatedStart> {
    if common.hook_event_name != "SubagentStart"
        || !bounded(&common.session_id, limits::EXTERNAL_ID_BYTES)
        || !bounded(&common.agent_id, limits::EXTERNAL_ID_BYTES)
        || !bounded(&common.agent_type, limits::EXTERNAL_ID_BYTES)
        || codex_turn.is_some_and(|turn| !bounded(turn, limits::EXTERNAL_ID_BYTES))
        || [
            common.parent_session_id,
            common.subagent_id,
            common.is_subagent,
        ]
        .contains(&Presence::Present)
    {
        return Err(Error::EventInvalid("delegated identity"));
    }
    if !bounded(&common.cwd, limits::PATH_BYTES) || !Path::new(&common.cwd).is_absolute() {
        return Err(Error::EventInvalid("cwd"));
    }
    let supplied = std::fs::canonicalize(&common.cwd).map_err(|_| Error::EventInvalid("cwd"))?;
    let actual = std::env::current_dir()
        .and_then(std::fs::canonicalize)
        .map_err(|_| Error::EventInvalid("process cwd"))?;
    if supplied != actual {
        return Err(Error::EventInvalid("cwd mismatch"));
    }
    Ok(DelegatedStart {
        identity: ChildIdentity {
            session_id: common.session_id,
            agent_id: common.agent_id,
        },
        cwd: actual,
    })
}

/// Parse the exact typed `SubagentStart` shape for one client adapter.
///
/// # Errors
/// Rejects malformed, oversized, contradictory or wrong-cwd input. A valid
/// event alone does not establish parent authority or successful delivery.
pub fn parse_delegated_start(adapter: ClientAdapter, bytes: &[u8]) -> Result<DelegatedStart> {
    if bytes.len() > 64 * 1024 {
        return Err(Error::EventTooLarge);
    }
    let source = std::str::from_utf8(bytes).map_err(|_| Error::EventInvalid("UTF-8"))?;
    match adapter {
        ClientAdapter::CodexV1 => {
            let event: CodexStart =
                serde_json::from_str(source).map_err(|_| Error::EventInvalid("Codex JSON"))?;
            checked(event.common, Some(&event.turn_id))
        }
        ClientAdapter::ClaudeV1 => {
            let event: ClaudeStart =
                serde_json::from_str(source).map_err(|_| Error::EventInvalid("Claude JSON"))?;
            if event.turn_id == Presence::Present {
                return Err(Error::EventInvalid("Claude turn_id"));
            }
            checked(event.common, None)
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use uuid::Uuid;

    use super::{ClientAdapter, parse_delegated_start};

    fn event(adapter: ClientAdapter) -> Value {
        let mut event = json!({
            "session_id": "parent",
            "agent_id": "child",
            "agent_type": "Explore",
            "cwd": std::env::current_dir().unwrap(),
            "hook_event_name": "SubagentStart",
        });
        if adapter == ClientAdapter::CodexV1 {
            event["turn_id"] = json!("turn");
        }
        event
    }

    #[test]
    fn typed_start_requires_both_session_and_child() {
        for adapter in [ClientAdapter::CodexV1, ClientAdapter::ClaudeV1] {
            let mut value = event(adapter);
            let parsed =
                parse_delegated_start(adapter, &serde_json::to_vec(&value).unwrap()).unwrap();
            assert_eq!(parsed.identity().session_id(), "parent");
            assert_eq!(parsed.identity().agent_id(), "child");
            for field in ["session_id", "agent_id", "agent_type"] {
                value[field] = Value::Null;
                assert!(
                    parse_delegated_start(adapter, &serde_json::to_vec(&value).unwrap()).is_err()
                );
                value = event(adapter);
            }
            for field in ["parent_session_id", "subagent_id", "is_subagent"] {
                value[field] = Value::Null;
                assert!(
                    parse_delegated_start(adapter, &serde_json::to_vec(&value).unwrap()).is_err()
                );
                value = event(adapter);
            }
            value["hook_event_name"] = json!("SessionStart");
            assert!(parse_delegated_start(adapter, &serde_json::to_vec(&value).unwrap()).is_err());
        }
    }

    #[test]
    fn duplicate_and_trailing_identity_fields_fail() {
        let adapter = ClientAdapter::ClaudeV1;
        let mut bytes = serde_json::to_vec(&event(adapter)).unwrap();
        bytes.pop();
        bytes.extend_from_slice(br#",\"agent_id\":\"other\"}"#);
        assert!(parse_delegated_start(adapter, &bytes).is_err());
        let mut bytes = serde_json::to_vec(&event(adapter)).unwrap();
        bytes.extend_from_slice(b"{}");
        assert!(parse_delegated_start(adapter, &bytes).is_err());
    }

    #[test]
    fn child_keys_isolate_all_identity_dimensions_and_text_collisions() {
        let adapter = ClientAdapter::ClaudeV1;
        let id = Uuid::new_v4();
        let mut value = event(adapter);
        let a = parse_delegated_start(adapter, &serde_json::to_vec(&value).unwrap()).unwrap();
        value["session_id"] = json!("parentchild");
        value["agent_id"] = json!("");
        assert!(parse_delegated_start(adapter, &serde_json::to_vec(&value).unwrap()).is_err());
        value["session_id"] = json!("parentc");
        value["agent_id"] = json!("hild");
        let b = parse_delegated_start(adapter, &serde_json::to_vec(&value).unwrap()).unwrap();
        let first = a.identity().store_hash(id, adapter);
        assert_ne!(first, b.identity().store_hash(id, adapter));
        assert_ne!(first, a.identity().store_hash(Uuid::new_v4(), adapter));
        assert_ne!(first, a.identity().store_hash(id, ClientAdapter::CodexV1));
    }
}
