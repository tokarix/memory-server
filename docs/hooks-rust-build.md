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

## Literal command grammar

The library accepts one literal argv, or one shell spelling that produces only
literal words. Single quotes preserve dollar/backtick characters as data;
double-quoted and unquoted expansion is rejected. Literal escaping and paths
with spaces are supported. It consumes the entire input, including trailing
text, and rejects NUL, oversized words/argv, substitutions, globs, tilde/brace
expansion, pipelines, redirection, heredocs, backgrounding, general conditions,
sequences, nested shell interpreters, eval, source, functions and opaque scripts.

One optional leading `cd [--] LITERAL && INVOCATION` is the sole compound form.
It cannot be combined with the client's cwd/workdir field or another cwd
transition. Filesystem resolution must subsequently prove that execution stays
inside the same protected binding/worktree/context and that logical/physical
cwd semantics agree. Parsing a literal path alone does not prove this.

Leading `NAME=value` assignments apply after the client environment field.
Literal `env` supports `-i`/`--ignore-environment`, `-u NAME`/`--unset NAME`,
`--unset=NAME`, `--`, then assignments followed by the pinned executable.
Options after assignments, env split-string/chdir forms and unknown environment
keys deny. Clear and unset operations remain explicit in provenance. Bare tool
names require the protected PATH mapping; changed/missing PATH requires an
exact approved absolute executable spelling. Maximum wrapper nesting is four.

`rustup run 1.94.0[-x86_64-unknown-linux-gnu] cargo|rustc|rustdoc ...` and Cargo's
`+1.94.0[-x86_64-unknown-linux-gnu]` selector are parsed only for the protected
installed pin. Installation/update switches, repeated selectors and conflicting
environment selectors deny. Filesystem resolution must also account for rustup
settings and toolchain-file overrides before claiming completeness.

Cargo build/check/test/clippy/run/bench/doc/install and built-in aliases
`b`, `c`, `t`, `r`, `d` are build-producing. Test/bench `--no-run` and program
`-- --list` still require build storage checks. Direct rustc/rustdoc are also
build-producing. Option names use closed per-action tables; unknown flags,
artifact-dir, Cargo rustc/rustdoc passthrough, browser opening, rustc `-o FILE`,
response files, explicit emit filenames, compiler wrappers and opaque linker,
incremental or save-temp flags are unsupported. Build classification is not
candidate completeness: source/configuration/output resolution is still needed.

The read-only allowlist is exact Cargo/rustc/rustdoc `--version`/`-V` and
`--help`/`-h`, plus built-in Cargo action `--help`/`-h` without configuration
overrides. Short-alias help is excluded because repository configuration can
replace those aliases. Clippy help is excluded because Clippy is an external
subcommand. `cargo help` is excluded because it can launch another program.
Metadata/fetch/update/clean/fmt, custom aliases, cargo extensions, make/just/nix,
trunk/wasm-pack, namespace/remote wrappers and arbitrary interpreters are not
read-only by default. Rustup itself is accepted only as the explicit wrapper.

## Read-only configuration library

The next library stage retains bounded regular config inputs and missing-file
evidence. It reads both `config` and `config.toml`, chooses the extensionless
file when present, and merges Cargo home followed by ancestors from root to
the discovery directory. Home is read once if it is also an ancestor. Normal
manifest selection leaves discovery at the proven physical execution cwd;
local `install --path` uses its selected source directory for discovery.
The caller must establish these directories before using the library.

The following table describes highest-to-lowest priority. Each selected path
retains its own provenance; output paths are not lexically simplified.

| Key | Precedence | Relative base |
| --- | --- | --- |
| Target | `--target-dir`, `CARGO_TARGET_DIR`, ordered `--config build.target-dir`, `CARGO_BUILD_TARGET_DIR`, discovered `build.target-dir`, workspace `target` | Dedicated flags/process environment/inline config: execution cwd; files: two levels above the defining file; default: workspace |
| Intermediate build | Ordered `--config build.build-dir`, `CARGO_BUILD_BUILD_DIR`, discovered `build.build-dir`, effective target | Same source bases; default preserves the effective target |
| Install root | `--root`, `CARGO_INSTALL_ROOT`, ordered/discovered `install.root`, Cargo home | Same source bases |
| Cargo home | `CARGO_HOME`, then `HOME/.cargo` | Process cwd for relative environment paths; ambiguous home aliases deny |
| Compiler flags | Encoded flag variable, ordinary flag variable, supported build configuration | Encoded flags split on unit separator; ordinary strings split on whitespace; configuration arrays concatenate in precedence order |
| Cargo child environment | Process value unless `[env]` is forced or the variable is absent | `relative=true` uses the defining value's file base; it does not reconfigure Cargo itself |

Dedicated target and install environment variables have special precedence.
The implementation uses the
[Cargo 1.94 context implementation](https://github.com/rust-lang/cargo/blob/rust-1.94.0/src/cargo/util/context/mod.rs)
for that distinction, alongside the
[configuration reference](https://doc.rust-lang.org/cargo/reference/config.html)
and [install discovery reference](https://doc.rust-lang.org/cargo/commands/cargo-install.html).
Fixtures are hand-authored and do not execute Cargo to discover paths.

Repeated literal `--config` files and single TOML assignments merge in order.
An existing file wins even when its spelling contains `=`; inline forms retain
absence evidence so later creation invalidates the analysis.
File overrides retain the same two-level base as discovered files. Scalar
types must agree; tables merge by key and arrays concatenate. `[env]` table
fields can merge across files while the `value` field retains its own base.
Compiler replacements, nonempty wrappers, changed child PATH and opaque or
output-changing extra Rust flags deny. Combined `CARGO_BUILD_RUSTFLAGS` and
configuration flag lists remain unsupported until their separate environment
array-merging semantics are covered.

This stage conservatively excludes includes, source/patch/path overrides,
credential-provider configuration, target cfg/linker/runner tables, profile
layout overrides, build-directory templates and unknown relevant keys. Display
configuration (`term`), unused custom aliases, incompatibility-report
frequency, cache cleaning frequency, new-project VCS choice and unused doc
browser configuration cannot select a build destination or launch a program
in this grammar. Only the documented keys of these unrelated namespaces are
ignored; future unknown keys deny. Definitions replacing `b`, `c`, `t`, `r`,
`d` or `clippy` deny, even when the current command does not use that alias.
Other custom alias invocations and browser opening are excluded by the grammar.

One shared read set permits 64 distinct paths, including absences, with
256 KiB per file, 1 MiB total, 16 TOML levels and the operation deadline.
The Linux provider walks descriptors without following symlink components,
rejects pseudo-filesystems and nonregular inputs, and reads only a checked
regular descriptor. Config paths through symlinks remain unsupported even
though the storage evaluator separately supports output-path symlinks.
File identities, contents, and missing prefixes are private. Rechecking every
observation catches replacement and newly created higher-precedence configs;
late reads fail. A domain-separated read-set digest reveals no raw contents,
but low-entropy values can still be guessed.

This library remains separate from the live gate. It does not establish
workspace/dependency completeness, toolchain selector agreement, locality or a
complete output inventory. It cannot enforce storage until those checks and
the evaluator/audit handoff are connected. Rechecks are point-in-time evidence;
same-UID tampering and the race after the final check remain.
