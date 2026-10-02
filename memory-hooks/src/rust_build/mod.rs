//! Non-executing, bounded analysis of the protected static Rust build contract.

pub mod cargo;
pub mod command;
pub mod config;
pub mod environment;
pub mod execution;
pub mod files;
pub mod inventory;
pub mod manifest;
pub mod paths;
pub mod toolchain;

/// Fixed failure categories. No variant can retain untrusted data.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Failure {
    /// The input exceeds a fixed analysis bound.
    Limit,
    /// Shell syntax cannot be reduced to the supported literal grammar.
    Syntax,
    /// Executable resolution is not covered by the protected declaration.
    Executable,
    /// The action or an option has unsupported semantics.
    Unsupported,
    /// An environment mechanism cannot be modeled completely.
    Environment,
    /// Effective working-directory semantics are ambiguous.
    Cwd,
    /// Toolchain selectors disagree with the installed protected pin.
    Toolchain,
    /// A required regular input cannot be read safely.
    Read,
    /// Cargo configuration is invalid or outside the modeled contract.
    Configuration,
    /// Source/workspace semantics cannot be resolved with the bounded grammar.
    Manifest,
    /// An observed input or absence changed before completion.
    Changed,
    /// Cooperative analysis completed after its operation deadline.
    Deadline,
}

/// Build-producing actions, including diagnostic forms that still compile.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BuildAction {
    /// Cargo build, including its built-in alias.
    Build,
    /// Cargo check, including its built-in alias.
    Check,
    /// Cargo test, including --list/--no-run.
    Test,
    /// Cargo clippy through the protected extension executable.
    Clippy,
    /// Cargo run; arbitrary user-code writes remain outside this contract.
    Run,
    /// Cargo bench, including --list/--no-run.
    Bench,
    /// Cargo doc without opening a browser.
    Doc,
    /// Cargo install; path completeness still requires local source resolution.
    Install,
    /// A direct rustc invocation, including --test.
    Rustc,
    /// A direct rustdoc invocation.
    Rustdoc,
}

/// Classification before filesystem/configuration resolution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    /// A pinned tool's exact --version or built-in --help spelling.
    AuditedReadOnly,
    /// An explicitly known operation that cannot launch a program.
    KnownNonExecution,
    /// Build-producing; this alone is not a complete candidate inventory.
    Build(BuildAction),
}
