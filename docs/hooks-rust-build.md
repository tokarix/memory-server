# Managed Rust execution contract

Schema v6 adds `rust_execution` to the protected operator configuration.
The non-live [v6 example](../memory-hooks-v6.toml.example) deliberately has
invalid boot/namespace/root pins. It does not install or activate a launcher.
Host v6 does not require `storage_execution`; a container exception still
requires the separately protected isolated-container attestation.

The execution declaration pins Codex CLI 0.158.0 or Claude Code 2.1.92,
Rust/Cargo 1.94.0 on x86_64 Linux, and one already installed toolchain. It
requires the existing synchronous delivery, gate and delegation declarations.
This does not expand the delegated lifecycle capabilities of either client.
Existing v1-v5 parsing and normalized fingerprints remain unchanged.

An operator must provision a sealed launcher whose execution tools and hook
worker share a mount namespace and filesystem root. Its inherited build
environment must equal the protected declaration. Tool resolution must use the
listed executables; shells must exclude functions, aliases, startup files and
login/interactive operation. Merely assigning a profile supplies none of this
evidence. Do not declare these capabilities for hosted, remote or interactive
continuations whose actual behavior has not been established.

The protected declaration includes a boot ID, mount namespace device/inode,
root device/inode, and the number of process ancestors between the supervised
worker and the managed client. Decision-time validation reads that client's
executable, process start identity, namespace/root and environment through
`/proc`. It rechecks each protected executable's metadata and SHA-256 contents
without executing it. Missing, inaccessible, replaced or late evidence denies.
The operator is responsible for verifying the installed executable versions
before provisioning; a version string does not discover or install a toolchain.

Executables must be outside protected workspaces, on trusted ownership/mode/ACL
paths. All new declarations and executable identities enter the v6 fingerprint,
including a domain-separated hash of inherited environment values. Activation
therefore invalidates older sessions. Hashing hides raw values, but does not
protect low-entropy values against guessing. Never publish the protected
configuration or private analysis inputs in audit records or diagnostics.

Client input retention is limited to bounded command/argv, literal execution
cwd, literal environment assignments/unsets, shell spelling and execution mode.
It retains no patch bodies or unrelated tool metadata. Duplicate execution
keys, ambiguous command/cwd forms, background/login/interactive modes and unknown
execution fields cannot establish a supported execution request. The event
limit remains 64 KiB; argv has at most 512 words of 4096 bytes, environment at
most 128 entries, with no NUL or prefix truncation.

This contract is an integration prerequisite. The bounded Rust resolver and
storage gate integration are separate implementation units of #90. Until they
are connected, the generic gate's behavior is unchanged and this declaration
alone must not be advertised as Rust storage enforcement.
