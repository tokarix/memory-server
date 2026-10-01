//! Bounded private execution input, without command or credential formatting.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Deserialize;
use serde_json::Value;

use crate::limits;

/// Literal command data. This does not establish execution authority.
pub struct ExecutionInput {
    pub(crate) command: CommandInput,
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) environment: BTreeMap<String, Option<String>>,
    pub(crate) shell: Option<PathBuf>,
}

/// The two supported client command spellings, kept private from diagnostics.
pub enum CommandInput {
    /// A shell spelling that still requires complete literal grammar parsing.
    Shell(String),
    /// A literal argument vector, with no shell interpretation.
    Argv(Vec<String>),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawInput {
    command: Option<String>,
    argv: Option<Vec<String>>,
    cwd: Option<String>,
    workdir: Option<String>,
    env: Option<BTreeMap<String, Option<String>>>,
    shell: Option<String>,
    login: Option<bool>,
    interactive: Option<bool>,
    tty: Option<bool>,
    run_in_background: Option<bool>,
    // Client presentation/timeout fields do not change execution semantics.
    description: Option<Value>,
    timeout: Option<Value>,
    timeout_ms: Option<Value>,
    yield_time_ms: Option<Value>,
    max_output_tokens: Option<Value>,
}

impl ExecutionInput {
    /// Command text for private analysis; never log or serialize it.
    #[must_use]
    pub const fn command(&self) -> &CommandInput {
        &self.command
    }
    /// Literal client execution directory, separate from callback identity.
    #[must_use]
    pub fn cwd(&self) -> Option<&std::path::Path> {
        self.cwd.as_deref()
    }
    /// Literal environment assignments and unsets; never log values.
    #[must_use]
    pub fn environment(&self) -> &BTreeMap<String, Option<String>> {
        &self.environment
    }
    /// Explicit shell spelling, which must match the protected contract.
    #[must_use]
    pub fn shell(&self) -> Option<&std::path::Path> {
        self.shell.as_deref()
    }

    /// Retain supported fields only. Errors are fixed and never include values.
    /// Callers must reject duplicate JSON keys before constructing the Value.
    ///
    /// # Errors
    /// Returns a static reason for ambiguous, oversized or unsupported input.
    pub fn from_value(value: &Value) -> Result<Self, &'static str> {
        let fields = value.as_object().ok_or("execution_fields")?;
        for key in [
            "command",
            "argv",
            "cwd",
            "workdir",
            "env",
            "shell",
            "login",
            "interactive",
            "tty",
            "run_in_background",
        ] {
            if fields.get(key).is_some_and(Value::is_null) {
                return Err("execution_fields");
            }
        }
        let raw: RawInput =
            serde_json::from_value(value.clone()).map_err(|_| "execution_fields")?;
        if [raw.login, raw.interactive, raw.tty, raw.run_in_background]
            .into_iter()
            .any(|flag| flag == Some(true))
        {
            return Err("execution_mode");
        }
        let command = match (raw.command, raw.argv) {
            (Some(command), None)
                if !command.is_empty() && command.len() <= 64 * 1024 && !command.contains('\0') =>
            {
                CommandInput::Shell(command)
            }
            (None, Some(words))
                if !words.is_empty()
                    && words.len() <= limits::EXECUTION_WORDS
                    && words
                        .iter()
                        .all(|word| word.len() <= limits::PATH_BYTES && !word.contains('\0')) =>
            {
                CommandInput::Argv(words)
            }
            _ => return Err("execution_command"),
        };
        let cwd = match (raw.cwd, raw.workdir) {
            (Some(_), Some(_)) => return Err("execution_cwd"),
            (value, None) | (None, value) => value,
        };
        let environment = raw.env.unwrap_or_default();
        if environment.len() > limits::EXECUTION_ENVIRONMENT
            || environment.iter().any(|(key, value)| {
                key.is_empty()
                    || key.len() > limits::PATH_BYTES
                    || key.contains(['\0', '='])
                    || value.as_ref().is_some_and(|value| {
                        value.len() > limits::PATH_BYTES || value.contains('\0')
                    })
            })
        {
            return Err("execution_environment");
        }
        for path in [cwd.as_ref(), raw.shell.as_ref()].into_iter().flatten() {
            if path.is_empty() || path.len() > limits::PATH_BYTES || path.contains('\0') {
                return Err("execution_path");
            }
        }
        let _ = (
            raw.description,
            raw.timeout,
            raw.timeout_ms,
            raw.yield_time_ms,
            raw.max_output_tokens,
        );
        Ok(Self {
            command,
            cwd: cwd.map(PathBuf::from),
            environment,
            shell: raw.shell.map(PathBuf::from),
        })
    }
}

/// Deserialize an object with unique keys, including nested environment maps.
pub(crate) fn unique_value<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Value, D::Error> {
    struct Unique(Value);
    impl<'de> Deserialize<'de> for Unique {
        fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            struct Visitor;
            impl<'de> serde::de::Visitor<'de> for Visitor {
                type Value = Value;
                fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    formatter.write_str("unique JSON data")
                }
                fn visit_map<M: serde::de::MapAccess<'de>>(
                    self,
                    mut map: M,
                ) -> Result<Value, M::Error> {
                    let mut values = serde_json::Map::new();
                    while let Some((key, Unique(value))) = map.next_entry::<String, Unique>()? {
                        if values.insert(key, value).is_some() {
                            return Err(serde::de::Error::custom("duplicate execution field"));
                        }
                    }
                    Ok(Value::Object(values))
                }
                fn visit_seq<S: serde::de::SeqAccess<'de>>(
                    self,
                    mut sequence: S,
                ) -> Result<Value, S::Error> {
                    let mut values = Vec::new();
                    while let Some(Unique(value)) = sequence.next_element()? {
                        values.push(value);
                    }
                    Ok(Value::Array(values))
                }
                fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Value, E> {
                    Ok(Value::String(value.to_owned()))
                }
                fn visit_bool<E: serde::de::Error>(self, value: bool) -> Result<Value, E> {
                    Ok(Value::Bool(value))
                }
                fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<Value, E> {
                    Ok(Value::from(value))
                }
                fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<Value, E> {
                    Ok(Value::from(value))
                }
                fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<Value, E> {
                    serde_json::Number::from_f64(value)
                        .map(Value::Number)
                        .ok_or_else(|| E::custom("number"))
                }
                fn visit_unit<E: serde::de::Error>(self) -> Result<Value, E> {
                    Ok(Value::Null)
                }
            }
            deserializer.deserialize_any(Visitor).map(Unique)
        }
    }
    Unique::deserialize(deserializer).map(|Unique(value)| value)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{CommandInput, ExecutionInput};
    use crate::limits;

    #[test]
    fn bounded_input_keeps_only_supported_execution_fields() {
        let input = ExecutionInput::from_value(&json!({"argv":["cargo", "build"], "workdir":"space here", "env":{"TMPDIR":null}, "login":false})).unwrap();
        assert!(matches!(input.command(), CommandInput::Argv(words) if words.len() == 2));
        assert_eq!(input.cwd().unwrap().to_str(), Some("space here"));
        assert_eq!(input.environment().get("TMPDIR"), Some(&None));
        for value in [
            json!({"command":"cargo build", "argv":["cargo"]}),
            json!({"command":"cargo build", "login":true}),
            json!({"command":"cargo build", "cwd":"a", "workdir":"b"}),
            json!({"command":"cargo build", "remote":true}),
            json!({"argv":vec!["x";limits::EXECUTION_WORDS+1]}),
            json!({"argv":["x".repeat(limits::PATH_BYTES+1)]}),
            json!({"command":"a\u{0}b"}),
        ] {
            assert!(ExecutionInput::from_value(&value).is_err());
        }
    }
}
