# Managed Rust execution contract

Schema v6 adds `rust_execution` to the protected operator configuration.
The non-live [v6 example](../memory-hooks-v6.toml.example) deliberately has
invalid boot/namespace/root pins. It does not install or activate a launcher.
Host v6 does not require `storage_execution`; a container exception still
requires the separately protected isolated-container attestation.

The execution declaration pins Codex CLI 0.158.0 or Claude Code 2.1.92,
Rust/Cargo 1.94.0, rustup 1.28.2 on x86_64 Linux, and one already installed toolchain. It
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
| Target selectors | Literal `--target` values, ordered `--config build.target`, `CARGO_BUILD_TARGET`, discovered `build.target`, host | Only the documented built-in triples; install ignores `build.target` and its environment form |
| Profile | Literal `--profile` or the command's release/debug selector, command default | Workspace profile fields, then merged Cargo configuration fields; no profile field selects protected policy |
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

## Cargo selection and layout library

`rust_build::cargo` extracts configuration, package, manifest, target, profile
and local-install selections from the already validated literal invocation.
It consumes option values separately from command names and stops Cargo
selection at a run/test/bench `--` separator. Repeated configuration overrides
retain their order. Duplicate scalar path/profile selectors and conflicting
release/debug versus explicit profiles deny. Remote installs stay build
producing but have no resolvable local layout.

Normal configuration discovery stays at the effective execution cwd even when
`--manifest-path` selects another package. Local `install --path` discovers
configuration at that source, selects its manifest/workspace separately and
uses the local workspace target default. Install root and its `bin` directory
are separate output roots; install compilation uses release by default or dev
with `--debug`. Explicit target selections still apply to install.

Built-in dev/test profiles use `debug`, while release/bench use `release`.
Supported custom profiles retain their own name regardless of which profile
they inherit. Workspace fields are overlaid by configuration fields, including
package/build overrides. Inheritance cycles, missing parents, root inheritance,
unknown fields, unstable directory names/backends and unsafe profile names
deny. Stable compilation settings cannot change these layout names. Profile
environment mechanisms outside the protected variable registry remain denied.
The explicit built-in target subset is the pinned host plus
`x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu` and
`wasm32-unknown-unknown`; `host-tuple` resolves to the pin's host triple.
JSON/future targets and ambiguous configuration-array/environment combinations
deny. No target list or profile is discovered by invoking a compiler.

The layout retains source, target, intermediate build, compiler temporary,
Cargo cache and install roles separately, including when paths coincide.
Host layout roots are included alongside explicit target roots for host units.
Artifact/profile/example roots and intermediate deps, artifact, example,
incremental, fingerprint, build-script and temporary roots are distinct.
Documentation and requested timing roots are added independently. Cargo home,
registry index/cache/source and git database/checkout roots are included even
for local-only sources because cache tracking/automatic cleanup can write there.
Compiler temporary selection uses the modeled child TMPDIR, with the pinned
Linux default `/tmp` when absent; it never invents a safer destination.

These are required static roots, **not CompleteBuild evidence**. Existing
descendants and direct compiler output trees are resolved by the following
inventory stage. Layout-derived child loader environment and final
backing/identity checks still require resolution before the gate can use them.
A parent root cannot cover an
existing nested mount or symlink. The library retains actual `..` components,
assigns stable candidate indices and rejects more than 64 directories or a
path beyond 4096 bytes. No layout or configuration read creates a directory.
Hand-authored fixtures cover all named Cargo actions, config/cwd/install
differences, child TMPDIR precedence, profiles, targets, duplicate-role paths,
bounds, replacements and unsupported forms without executing a proposal.
The layout and profile rules follow the pinned
[Cargo 1.94 layout source](https://github.com/rust-lang/cargo/blob/rust-1.94.0/src/cargo/core/compiler/layout.rs)
and [profile source](https://github.com/rust-lang/cargo/blob/rust-1.94.0/src/cargo/core/profiles.rs).

## Directory inventory library

`rust_build::inventory` expands the Cargo layout roots through every existing
output descendant, including unknown package/build-script/incremental hashes,
documentation crate directories, caches and install directories. Each existing
directory receives its own output candidate so a nested mount gets its own
backing assessment. Missing roots retain absence evidence. A directory listed
as existing but missing when visited is a changed input. The shared read-set,
64-directory ceiling, 512-entry listings, 16 recursive levels, path/byte bounds
and original operation deadline apply throughout; no overflow is truncated.

Output listings use metadata-only no-follow descriptors. Regular files must
have the directory's mount ID; directory mount transitions are retained for
separate assessment. Symlink files/directories, FIFOs, devices, sockets,
individual file mounts and missing kernel mount-ID evidence deny. File contents
are not opened, so oversized artifacts cannot turn a metadata probe into an
unbounded read or block on a FIFO. Ordered names, file kind/identity, regular
file metadata and mount IDs enter private recheck evidence. New entries,
replacement, failed reads and late completion invalidate the analysis.
Source-glob and stronger output listings at a coincident path are retained
separately and both rechecked; each consumes one of the 64 observation slots.
An output observation cannot substitute for a regular input file or executable.

Direct rustc initially requires a literal `--out-dir DIRECTORY`; rustdoc
requires `-o DIRECTORY` or `--output DIRECTORY`. The source must be an existing
bounded regular file and its contents/identity are retained privately. Source,
primary output, nested output and modeled compiler temporary roles stay
separate. Directory output and ordinary `--test`/filename-free `--emit` forms
are supported; default outputs, repeated output selectors, individual files,
response files, short `-o=...` spelling, unknown editions/types and unsafe crate
names deny. The generic command grammar continues to reject opaque compiler
and linker flags before inventory resolution.

The inventory covers the documented static directory contract at the check
instant. Normal future directories inherit an assessed existing prefix;
post-check creation/replacement/mount races and arbitrary user-code writes
remain outside this proof. It does not assert execution locality, authorize a
command or substitute for authoritative storage evaluation. The claimed-worker
and audit handoff remain pending. Fixtures cover direct compiler/test/doc/emit
forms, spaces, unknown existing output hashes, coincident roles, creation and
replacement, metadata-only large artifacts, mounted-file rejection and
FIFO/symlink denial without privileged mounts or executing a proposal.

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

## Local source selection library

The standalone source stage searches execution-cwd ancestors for the selected
`Cargo.toml`, or reads a literal `--manifest-path` relative to execution cwd.
That selection does not move ordinary Cargo configuration discovery. Workspace
roots come from the manifest's `[workspace]`, explicit `package.workspace`, or
the nearest ancestor workspace. Standalone packages and nested workspaces use
their own roots; a nested workspace cannot also be an ordinary member of its
outer workspace in this subset. Policy and callback identity remain external
prerequisites: source selection cannot change either one.

Root packages, virtual workspaces, `members`, `default-members`, `exclude`,
implicit in-workspace path-dependency members, exact `--package` names and
`--workspace` selection are modeled separately. The supported member glob has
exactly one whole path component `*`, for example `crates/*`. A bounded, sorted
directory listing provides the matches. Symlink/non-directory matches,
recursive globs, partial-component patterns, wildcard package selectors and
package-ID specifications deny. Literal member/dependency path joins preserve
parent components; physically equivalent aliases that cannot be established
from retained input evidence remain unsupported.

Normal, build, development, optional and target-specific local dependencies
form a conservative union of source roots, including external local packages.
Workspace dependency inheritance uses the workspace root as its path base.
Missing manifests, cyclic dependencies, duplicate member names, invalid
defaults, source replacements and unknown execution-related manifest keys
deny. Registry/git dependencies and artifact dependencies remain unsupported
in this local-only library; no future downloaded source tree is assumed safe.
Every workspace member must fit this local-only subset, even if package
selection excludes it, so automatic membership cannot hide a source path.

Directory observations share the same 64-input, 256-KiB-per-input, 1-MiB-total
budget and deadline as file observations. Listings contain at most 512 UTF-8
basenames; unrepresentable or ambiguous names deny. Rechecks cover contents,
directory identities, new glob matches and newly created nearer manifests.
The read-set digest is now domain-separated as version 2 and includes the
input kind so a directory observation cannot substitute for a file read.
The real provider reads only descriptors for checked directories; it never
follows glob-member symlinks or launches Cargo, scripts or compiler programs.

The source inventory is stable and private. It supplies the workspace default
target base and selected package metadata for later output-layout resolution.
It does not resolve toolchain selectors, profiles, remote caches, concrete
build outputs or execution locality, and remains disconnected from the gate.
The [Cargo workspace reference](https://doc.rust-lang.org/cargo/reference/workspaces.html)
and [dependency reference](https://doc.rust-lang.org/cargo/reference/specifying-dependencies.html)
describe the source semantics underlying the tested Cargo 1.94 subset.

## Rustup selector evidence library

The standalone selector stage uses the parsed invocation, proven physical
execution cwd, and Cargo's separately computed child environment. It reads
`RUSTUP_HOME/settings.toml` (or `HOME/.rustup/settings.toml`) without invoking
rustup. Missing settings deny rather than initializing a file. Settings must
use metadata version `12`, the pinned host triple if specified, and
`auto_install = "disable"`. Explicit self-update settings must be `disable`.
Unknown settings and custom/path toolchains deny. This first subset requires
confirmed absence of `/etc/rustup/settings.toml`; operator fallback configuration
remains unsupported. The v6 declaration's required `rustup_semantics` pin enters
its normalized fingerprint alongside the existing executable content identities.

| Selector | Precedence and agreement |
| --- | --- |
| Explicit Cargo `+TOOLCHAIN` or `rustup run` | Highest; the grammar has already checked the protected release |
| Effective `RUSTUP_TOOLCHAIN` | Next; Cargo child selectors must also agree with the protected release |
| Directory settings override or toolchain file | Search cwd ancestors from nearest to farthest; the directory override wins a file at the same level |
| `rust-toolchain` versus `rust-toolchain.toml` | Read both and retain both observations; extensionless wins when present |
| Default toolchain | Used only when no higher selector exists; it must identify the protected release |

The nearest directory/file selector must agree even when an explicit or
environment selector shadows it. This stricter agreement rule deliberately
denies conflicting repository pins. Farther shadowed files are observed without
interpreting them. Toolchain files accept the legacy one-line ASCII name only
for `rust-toolchain`, or a TOML `[toolchain]` with `channel` and optional empty
`components`/`targets`. Nonempty component/target lists, toolchain profiles and
custom paths deny because they can request installation or introduce unpinned
tools. Only `1.94.0` and its exact protected host-qualified spelling are accepted.

Selector inputs and both missing filename alternatives join the existing shared
read set. Physical spelling, regular-file rules, directory evidence, the
64-input/byte/depth budgets and the operation deadline apply. Rechecks detect new
nearer files, changed settings and installed-directory replacement. Cargo child
`HOME` or `RUSTUP_HOME` changes deny rather than silently selecting a second
compiler context. A read-provider fixture cannot supply execution authority.

The installed tree must contain real `bin` and `lib` directories. Its existence
is only selector evidence: a separate trusted-path check hashes installed
Cargo/rustc/rustdoc/Clippy executables and compares their identities with the
protected `installed_executables` inventory. Missing, substituted or differently
installed executables fail that check. The v6 `sealed-executables-v2` contract
has distinct entrypoint and installed inventories, described below. Grammar
recognition of `+TOOLCHAIN` alone cannot authorize it. The later gate
must combine installed identity checks with process locality, exact binding,
complete candidates, final read-set/deadline rechecks and locked audit completion.
No live storage-enforcement claim follows from this library stage.

Hand-authored tests cover selector order/proximity, filename preference,
conflicting pins, additions, settings/fallback exclusions, child changes, missing
installation, read-set replacement/bounds/deadlines, fixed redacted failures and
nonexecution sentinels. Trusted regular executable fixtures prove matching
secondary identities pass, replacement denies and no executable is launched.
These rules follow the
[rustup 1.28.2 selector implementation](https://github.com/rust-lang/rustup/blob/1.28.2/src/config.rs)
and [settings format](https://github.com/rust-lang/rustup/blob/1.28.2/src/settings.rs),
with the conservative exclusions described above. They do not remove the
same-UID limitations or the race after the final point-in-time check.

## Entrypoints and generated environments

The required `installed_executables` map pins real `cargo`, `rustc`, `rustdoc`,
`cargo-clippy` and `clippy-driver` under the declared Rustup home and toolchain.
The existing `executables` map pins the client, shell, env and rustup plus the
five primary Rust entrypoints. Those five must all name either the installed
tools or the corresponding Cargo-home `bin` proxies. Mixed entrypoint modes,
missing inventories and unknown mapping versions deny. These paths and both
inventories' content/metadata identities enter the v6 fingerprint; versions
1-5 keep their previous parsing and fingerprints. This is a non-live v6
declaration change, with execution evidence version 2.

The execution-model library retains each wrapper's exact spelling and effective
environment. For a bare entrypoint it checks every earlier PATH candidate and
the selected protected file; absolute entrypoints must match the protected
path. Search has at most 32 directories, and probes share the 64-input deadline
budget with manifests/configuration/selectors. Relative/empty search entries,
logical traversal, inaccessible inputs, non-executable files, symlinks and
special files deny. Executable probes read metadata only, including for large
binaries. The separate protected identity check reads content without executing
it and checks metadata again after reading. A new shadowing file, replacement
or changed observation invalidates the shared read set.

Rustup `run` accepts a literal bare `cargo`, `rustc` or `rustdoc` and chooses the
installed binary, rather than resolving that inner word through inherited PATH.
An installed direct Cargo entrypoint does not accept the proxy's `+TOOLCHAIN`
syntax. The managed grammar excludes absolute inner `rustup run` program paths
and Rustup's PATH fallback for missing installed tools.

The Linux Rustup 1.28.2 model applies its generated values before Cargo reads
configuration. It handles a secondary compiler proxy hop separately:

| Input layer | Modeled effects |
| --- | --- |
| Protected/client/assignment/env wrapper | Literal inputs with their original origins; loader and recursion mechanisms are forbidden |
| Primary rustup proxy or `rustup run` | Absolute Cargo home; Cargo-home `bin` PATH insertion; installed `lib` loader path; exact Rustup home/toolchain; recursion count 1 |
| Cargo `[env]` | Child inputs only, including force/relative temporary values; cannot replace PATH, home, selector, loader or recursion evidence |
| Secondary compiler proxy | The same home/toolchain values and loader insertion; recursion count 2 after a primary hop, otherwise 1 |
| Direct installed executable | Preserves inputs without manufacturing rustup-generated values |

Clippy additionally searches Cargo-home `bin` with Cargo's own unique insertion
rule. Its protected extension receives the installed Cargo path in generated
`CARGO`, selects the installed sibling `clippy-driver`, and invokes that Cargo
directly with generated workspace-wrapper and empty Clippy-argument values.
With proxies, the extension adds a second rustup hop; external dependency rustc
proxies can add a third. Workspace-driver and dependency-compiler environments
are retained separately. Global options before `clippy`, passthrough/fix forms,
and configuration that replaces the generated wrapper are excluded. Ambient
`CARGO`, `CLIPPY_*` and `SYSROOT` mechanisms cannot be hidden outside the sealed
declaration. The static model still precedes Cargo's layout-derived loader
additions, which require the later complete output stage. These rules follow
the pinned [Cargo external launcher](https://github.com/rust-lang/cargo/blob/rust-1.94.0/src/bin/cargo/main.rs)
and [Clippy launcher](https://github.com/rust-lang/rust/blob/1.94.0/src/tools/clippy/src/main.rs).

Rustup's unique PATH insertion preserves an existing entry's position. It does
not move Cargo-home `bin` ahead of an earlier entry. The resolver models that
rule and checks the resulting search, rather than assuming insertion proves
tool identity. Generated loader/recursion values cannot be supplied through
client, wrapper or Cargo configuration input. Cargo's process, child and compiler
environments remain separate; child target-directory variables do not select
Cargo's own output directory. Private digests cover invocation spellings,
arguments, environment origins and explicit removals. Digests have the same
guessability limits as other audit hashes.

Generated PATH/loader entries containing a literal colon are unsupported:
Unix path-list encoding cannot represent that directory as one entry. The
resolver denies instead of modeling an insertion Rustup would not perform.

The execution fixtures prove separate proxy/installed replacement checks,
absence invalidation, primary versus secondary generation, direct versus rustup
entrypoints, force/relative child inputs, fixed failures, shared limits and
nonexecution. The metadata probe fixture rejects FIFOs, symlinks and non-executable
files without reading or running them. These rules follow
[Rustup 1.28.2 process construction](https://github.com/rust-lang/rustup/blob/1.28.2/src/toolchain.rs)
and its [unique path insertion](https://github.com/rust-lang/rustup/blob/1.28.2/src/env_var.rs).
This library still supplies no locality, claim or policy authority. The worker
must combine it with decision-time locality, exact binding/pack linkage,
complete output inventories, storage evaluation and locked audited completion.
Storage enforcement remains disconnected until that integration is complete.

## Checked execution cwd handoff

`TrustedHooksConfig::resolve_execution` retains the original callback binding
and resolves a supported literal cwd independently. Its result supplies a
private checked execution binding for the later analyzer/storage handoff. It
never changes the hook process cwd, adopts another project/profile or rewrites
session identity. Without a literal transition, execution uses the callback's
canonical cwd.
Relative and absolute paths with spaces and agreeing parent components are
accepted only within the exact same repository, checkout/worktree, project,
context and protected configuration fingerprint. Another linked worktree
sharing a Git common directory still denies.

The handoff rejects symlink components, missing/non-directory locations,
logical/physical cwd disagreement, oversized or invalid paths, unmapped or
newly nested repositories and movement into another approved binding. It
compares physical canonicalization with the managed shell's logical path only
for cwd agreement; build-output joins still retain filesystem components for
the storage evaluator. The established hardened Git identity probes remain
the sole executable identity-resolution helper. Proposed tools, compilers and
repository programs are never launched by the handoff or the Rust analyzer.

Callback, execution and worktree directory device/inode evidence is retained
privately and checked immediately after resolution and during the final
handoff recheck. Directory replacement, changed literal resolution or a new
nested repository denies. Every identity probe now respects the remaining
shared deadline; rechecks cannot reset that deadline. Integration still must
combine these observations with the exact claim, pack, execution contract,
input read set, storage report and locked audit completion. The live gate has
not yet been wired to this API, and same-UID/post-check race limits remain.

Real identity fixtures cover same-cwd/nested/parent transitions, literal spaces,
unchanged callback cwd, linked-worktree and cross-binding rejection, symlinks,
new nested repositories, directory/root replacement, changed protected
fingerprints, input bounds and deadline expiry. These fixture setup commands
are distinct from analyzed build proposals.
