//! Fixed v1 input, storage, and deadline limits.

use std::time::Duration;

/// Maximum trusted TOML bytes.
pub const CONFIG_BYTES: usize = 256 * 1024;
/// Maximum number of configured bindings.
pub const BINDINGS: usize = 1024;
/// Maximum bytes in one supplied path.
pub const PATH_BYTES: usize = 4096;
/// Maximum guardrail project bytes.
pub const PROJECT_BYTES: usize = 256;
/// Maximum external client or session identifier bytes.
pub const EXTERNAL_ID_BYTES: usize = 1024;
/// Maximum encoded record bytes.
pub const RECORD_BYTES: usize = 128 * 1024;
/// Maximum combined stdout and stderr bytes per Git probe.
pub const PROBE_BYTES: usize = 64 * 1024;
/// Per-probe and lock deadline.
pub const SHORT_DEADLINE: Duration = Duration::from_secs(5);
/// Overall identity resolution deadline.
pub const RESOLVE_DEADLINE: Duration = Duration::from_secs(20);
