//! Stable, redacted error reasons for administrative callers.

use std::fmt;

/// A failure reason that does not contain repository text or credentials.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    /// Config path, ownership, permissions, or ACL are untrusted.
    ConfigUntrusted(&'static str),
    /// Config syntax, shape, or semantic content is invalid.
    ConfigInvalid(&'static str),
    /// TOML parsing failed at a bounded line and column, without source text.
    ConfigSyntax { line: usize, column: usize },
    /// Config exceeded its fixed limit.
    ConfigTooLarge,
    /// No explicit binding matches the location.
    IdentityUnmapped,
    /// More than one binding could match.
    IdentityAmbiguous,
    /// Git or directory metadata is malformed or unsupported.
    IdentityInvalid(&'static str),
    /// Identity metadata changed during resolution.
    IdentityChanged,
    /// Git exceeded its deadline.
    GitTimeout,
    /// State path, owner, permissions, ACL, or filesystem is unsafe.
    StateInsecure(&'static str),
    /// A committed state record is malformed.
    StateCorrupt,
    /// A record belongs to another binding or version.
    StateScopeMismatch,
    /// A state lock exceeded its deadline.
    StateLockTimeout,
    /// Installation manifest, journal, or pinned identity is invalid.
    InstallationInvalid(&'static str),
    /// Installation is changing, retired, or awaiting explicit repair.
    InstallationInactive,
    /// Active configuration differs from the committed installation authority.
    InstallationMismatch,
    /// A client session identifier or current generation is invalid.
    SessionInvalid(&'static str),
    /// The bounded client event is malformed or outside the supported contract.
    EventInvalid(&'static str),
    /// Client event exceeds its fixed input bound.
    EventTooLarge,
    /// The fresh guardrail fetch failed; the reason is allowlisted.
    GuardrailsUnavailable(&'static str),
    /// A newer generation or installation activation superseded this work.
    SessionStale,
    /// The current head or exact snapshot payload is unavailable or corrupt.
    SnapshotInvalid(&'static str),
    /// Exact accepted output exceeds its framing bound.
    OutputTooLarge,
    /// An operation failed; the OS code is safe to disclose.
    Io(&'static str, Option<i32>),
    /// A write may have been renamed but not durably synced.
    DurabilityUncertain,
    /// Platform cannot provide required guarantees.
    UnsupportedPlatform,
}

/// Result with a stable, redacted error.
pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// Return the stable machine-readable reason code.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::ConfigUntrusted(_) => "config_untrusted",
            Self::ConfigInvalid(_) | Self::ConfigSyntax { .. } => "config_invalid",
            Self::ConfigTooLarge => "config_too_large",
            Self::IdentityUnmapped => "identity_unmapped",
            Self::IdentityAmbiguous => "identity_ambiguous",
            Self::IdentityInvalid(_) => "identity_invalid",
            Self::IdentityChanged => "identity_changed",
            Self::GitTimeout => "git_timeout",
            Self::StateInsecure(_) => "state_insecure",
            Self::StateCorrupt => "state_corrupt",
            Self::StateScopeMismatch => "state_scope_mismatch",
            Self::StateLockTimeout => "lock_timeout",
            Self::InstallationInvalid(_) => "installation_invalid",
            Self::InstallationInactive => "installation_inactive",
            Self::InstallationMismatch => "installation_mismatch",
            Self::SessionInvalid(_) => "session_invalid",
            Self::EventInvalid(_) => "event_invalid",
            Self::EventTooLarge => "event_too_large",
            Self::GuardrailsUnavailable(_) => "guardrails_unavailable",
            Self::SessionStale => "session_stale",
            Self::SnapshotInvalid(_) => "snapshot_invalid",
            Self::OutputTooLarge => "output_too_large",
            Self::Io(_, _) | Self::DurabilityUncertain => "io",
            Self::UnsupportedPlatform => "unsupported_platform",
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let hint = match self {
            Self::IdentityUnmapped | Self::IdentityInvalid(_) | Self::IdentityChanged => {
                "; ask the operator to configure or rebind this location"
            }
            _ => "",
        };
        match self {
            Self::ConfigSyntax { line, column } => {
                write!(
                    formatter,
                    "{}: TOML line={line} column={column}",
                    self.code()
                )
            }
            Self::Io(operation, code) => {
                write!(formatter, "{}: {operation}: os_error={code:?}", self.code())
            }
            Self::ConfigUntrusted(field)
            | Self::ConfigInvalid(field)
            | Self::IdentityInvalid(field)
            | Self::StateInsecure(field) => write!(formatter, "{}: {field}{hint}", self.code()),
            Self::InstallationInvalid(field) => write!(formatter, "{}: {field}", self.code()),
            Self::SessionInvalid(field)
            | Self::EventInvalid(field)
            | Self::GuardrailsUnavailable(field)
            | Self::SnapshotInvalid(field) => {
                write!(formatter, "{}: {field}", self.code())
            }
            _ => write!(formatter, "{}{hint}", self.code()),
        }
    }
}

impl std::error::Error for Error {}

/// Convert an I/O error without retaining its potentially path-bearing message.
pub(crate) fn io(operation: &'static str, error: &std::io::Error) -> Error {
    Error::Io(operation, error.raw_os_error())
}
