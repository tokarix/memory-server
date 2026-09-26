# Trusted hooks foundation

`memory-hooks` is a Rust library and administrative binary for later hook
adapters. It validates one operator-selected configuration, resolves an actual
location to a canonical local repository binding, and offers private typed
state transactions. It is not wired into the existing `hooks/*.sh` scripts and
does not fetch guardrails, handle client events, run pending commands, emit a
client hook response, or enforce a decision. The old scripts retain their own
`config.toml`/`MEMORY_SERVER_CONFIG` behavior and legacy `/tmp` state; neither
is imported or migrated by this crate.

## Managed installation

Install the helper executable and [example configuration](../memory-hooks.toml.example)
outside every configured workspace. A managed launcher must supply a fixed
absolute `--config` argument that repository text, model output, and tool
arguments cannot change. The process cannot prove who authored its argv. Pin
the helper binary, configuration and their parent directories with OS access
controls. Restrict configuration to the effective user or trusted root owner,
mode `0600` or owner-read-only equivalent, with no symlink component or
group/other-writable parent. The loader also rejects POSIX ACLs or filesystems
where ACL absence cannot be established. It never repairs permissions.

The `git_executable` must be an absolute, operator-selected executable outside
configured workspaces. Its resolved path and parent components are checked for
ownership, mode, symlinks and ACLs before use. A repository-controlled Git
binary is invalid. `state_root` must be absolute and outside all declared
workspace scopes. The state store opens it through directory handles and
creates a missing managed leaf with mode `0700`. It rejects unsafe existing
objects. `check-config` inspects configuration without initializing state.

Build and inspect the foundation with:

```sh
cargo build -p memory-hooks
memory-hooks --version
memory-hooks check-config --config /etc/memory-hooks/hooks.toml
memory-hooks resolve --config /etc/memory-hooks/hooks.toml --cwd /srv/work/example
```

`resolve` prints a version-one JSON object containing the identity kind,
SHA-256 digests of the canonical identity, worktree and effective cwd, the
explicit `guardrails_project`, complete context and fingerprint. It does not
print raw filesystem paths. Errors go to stderr with a nonzero exit code.
Both CLI commands reject unknown or duplicate arguments; the caller cannot
supply a project, profile, phase, language, or tool override. The CLI validates
the supplied `--cwd`; later adapters must establish the actual effective cwd
of a tool themselves. This command is not proof of where an arbitrary pending
command will execute.

## Schema and binding

The typed TOML schema requires `schema_version = 1`, `state_root`,
`git_executable`, and one or more `[[bindings]]`. Unknown keys, including
`api_token` and transport options, fail. No credentials or endpoint transport
belong to this version. A Git binding requires `kind = "git"`, a canonicalizable
absolute `common_dir`, exactly one of `primary_worktree` or
`bare_backed = true`, a unique label, `guardrails_project`, and a
`[bindings.context]` with a valid `profile`. A directory binding requires
`kind = "directory"`, an absolute root, and the same project/context fields.
`allow_subdirectories` defaults to `false` and must be explicitly true for
descendants. A project literally named `general` is accepted when explicitly
bound.

`ResolutionContext` is the shared `memory-common` schema. Its `profile` and
optional `phase` use canonical identifiers, and optional `language` and
`tool` are sorted sets. An omitted dimension means unknown; an explicit empty
set means known empty. Event data cannot fill unknown dimensions. There is
one context per canonical identity in one configuration. Use separate
operator-selected configuration files to select different profiles for the
same repository; untrusted events cannot choose a file or profile. Duplicate
labels, duplicate canonical identities and overlapping directory scopes fail.
There are no repository-relative defaults, path searches, environment or
tilde expansion, includes, shell interpolation, basename or remote inference,
or implicit workstation/container/general profile.

An ordinary checkout and its registered linked worktrees share the canonical
Git common-directory identity, while the actual canonical top-level and cwd
remain separate. Primary checkout membership is checked against the configured
primary root and its real `.git` directory or gitfile. Linked worktrees require
matching registration, gitfile, reciprocal `gitdir` pointer, and `commondir`.
Git is probed twice and metadata changes detected between observations fail.
The nearest repository boundary wins: a nested repository or submodule needs
its own binding, even inside a configured parent. Symlinked cwd aliases follow
their canonical target. Directory bindings apply only after Git boundaries,
including broken `.git` markers, have been ruled out. Git errors never become
directory fallback. Bare repository directories are not working checkouts.

This identity is local to the installation. Distinct clones stay distinct even
with identical basename, content, branch, and origin URL. Moving or replacing
Git administration metadata requires operator rebinding; no automatic path or
remote migration occurs. A fingerprint changes when normalized trust fields,
identity mappings, project, context, or state settings change. TOML whitespace
and key order do not change it. The `v1:` SHA-256 fingerprint is an invalidation
and isolation token, not an authenticity signature.

Every Git probe uses literal argv and the configured executable with the
canonical cwd. The child starts with an empty environment and only these fixed
values: `LC_ALL=C`, `PATH=/nonexistent`, `GIT_CONFIG_NOSYSTEM=1`,
`GIT_CONFIG_GLOBAL=/dev/null`, `GIT_TERMINAL_PROMPT=0`, and
`GIT_OPTIONAL_LOCKS=0`. Ambient `GIT_*`, `MEMORY_*`, `HOME`,
`XDG_CONFIG_HOME`, `PATH`, and `TMPDIR` do not redirect identity. Probes
disable system/global Git configuration and never use a shell, hook, alias,
external command, or network operation. They do not set `safe.directory=*`.
Malformed, oversized, partial, or timed-out output fails without logging Git
stderr.

## State and limits

The state API derives one opaque key from domain-separated, length-delimited
SHA-256 encodings of repository, binding, client and session data. Filenames
have fixed prefixes and lowercase hex digests. Hashing protects path syntax;
guessable identifiers are not secret. A version-one envelope binds each
bounded typed record to its scope and format version. Corrupt, truncated,
oversized and wrong-scope records fail rather than become empty state.

Transactions use a stable per-key advisory lock with a five-second acquisition
deadline. A writer creates an exclusive same-directory temporary file at
`0600`, writes bounded bytes, flushes and syncs it, atomically renames it over
the checked destination, then syncs the parent directory. A post-rename sync
error means durability is uncertain. Failed pre-rename writes leave the old
complete record. Only the transaction's own temporary name is cleaned up;
orphan temporary files are never read as committed records. Lock files remain
in place across transactions. The API supplies storage primitives, not a
guardrail snapshot or emitted/prepared lifecycle.

Version-one fixed limits are 256 KiB config, 1,024 bindings, 4 KiB per path,
256 bytes per project, 1 KiB per external client/session ID, 128 KiB per
record, 64 KiB combined Git stdout/stderr per probe, five seconds per probe
and lock, and 20 seconds for a complete resolution. The shared
`ResolutionContext` retains its 32-item and 128-byte identifier bounds.
Inputs over a limit are rejected without truncation or unbounded hashing.

Linux local ext4, XFS, Btrfs and tmpfs are the supported state filesystems.
They must provide meaningful ownership/mode and POSIX ACL reporting,
advisory `flock`, same-directory atomic rename and file/directory `fsync`.
The state-root creation path also requires `/proc/self/fd` to chmod a newly
created inode safely under a restrictive umask. Unsupported or ambiguous
semantics fail rather than degrade to
unchecked state. The workstation workspace is on virtiofs and does not expose
POSIX ACL absence for all home-directory components, so permission-sensitive
fixtures run under the user's private runtime directory; Cargo targets and
compiler temporary files remain on persistent disk. Other platforms currently
return `unsupported_platform`.

Stable error codes are `config_untrusted`, `config_invalid`,
`config_too_large`, `identity_unmapped`, `identity_ambiguous`,
`identity_invalid`, `identity_changed`, `git_timeout`, `state_insecure`,
`state_corrupt`, `state_scope_mismatch`, `lock_timeout`, `io`, and
`unsupported_platform`. Diagnostics contain only bounded operation/field
names and OS error codes, with a rebind hint for identity failures. They do
not include TOML source, raw Git stderr, raw paths, environment values, tool
payloads, credentials, or credential-bearing URLs.

The supported interface gives the model no project/profile override. This
does not protect user-owned config, helper binaries, Git metadata, or private
state from arbitrary code running as the same effective user. Ownership
checks, hashes and advisory locks coordinate cooperating callers, not hostile
same-UID processes. Stronger isolation requires managed installation and OS
access controls or a separate privileged service. Git metadata is checked
before return but is not an atomic filesystem snapshot.
