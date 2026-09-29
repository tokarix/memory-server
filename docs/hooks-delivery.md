# Managed SessionStart guardrail delivery

`memory-hooks session-start` consumes one top-level Codex or Claude Code
`SessionStart` event, fetches a fresh authoritative mandatory pack, writes one
JSON response with its exact `GuardrailPack::publication()` in
`hookSpecificOutput.additionalContext`, and records local emission evidence.
It does not gate later actions by itself. A [v3 managed pre-tool gate](hooks-gate.md)
revalidates the current exact snapshot and fresh authoritative pack for every
observed guarded event. Optional bootstrap, recall, transcript capture and pre-compact
scripts have separate state and failure budgets and cannot rescue an invalid
mandatory snapshot.

## Provisioning and activation

Install the helper and a protected [v2 TOML configuration](../memory-hooks-v2.toml.example)
or [v3 gate configuration](../memory-hooks-v3.toml.example)
outside every workspace. Use a fixed absolute control directory for each
installation. The trusted launcher supplies its generated UUID and adapter;
events, environment variables, repository content, and the active config
cannot move this anchor. Its parent and the config/binary path need the
ownership, mode and ACL controls in [the trust guide](hooks-trust.md).

```sh
/opt/memory-hooks/bin/memory-hooks check-config --config /etc/memory-hooks/codex.toml
/opt/memory-hooks/bin/memory-hooks init-installation --installation /var/lib/memory-hooks/codex-control --client codex-v1
# Copy the installation_id from the initialization JSON into protected launcher settings.
/opt/memory-hooks/bin/memory-hooks activate-installation --installation /var/lib/memory-hooks/codex-control --installation-id 11111111-1111-4111-8111-111111111111 --client codex-v1 --config /etc/memory-hooks/codex.toml
/opt/memory-hooks/bin/memory-hooks installation-status --installation /var/lib/memory-hooks/codex-control --installation-id 11111111-1111-4111-8111-111111111111 --client codex-v1
```

The UUID above is a placeholder; use the one generated once by
`init-installation`. Initialization refuses an existing anchor and startup
never initializes one. `check-config` only validates; it does not activate or
emit. Use a separate protected config and anchor for a Claude installation,
with `adapter = "claude-v1"` and no `additional_context_limit` key.

All delivery-affecting changes, including config path, binding, project,
context, profile, endpoint, credential revision, adapter contract and
`state_root`, require `activate-installation` with a new protected config.
Do not edit an active config in place. Activation first commits a Changing
epoch and fresh nonce under the fixed installation lock. It then validates
the config/root and commits Active for that epoch. Failure leaves old heads
retired and the installation Changing. A root may be on a different supported
filesystem. Restoring an old root/config is another activation; old payloads
remain on disk but confer no authority. Every session needs another fresh
successful `SessionStart`. A detected in-place mismatch likewise advances to
Changing and cannot be undone by reverting file bytes.

For recovery, repair an unresolved installation transition with
`repair-installation`, then activate a validated config and start fresh client
sessions. An unresolved session-head transition can be advanced with
`repair-session --session-id <exact-client-session-id>` on the pinned anchor;
it commits a fresh Pending tombstone at a higher sequence, never an Emitted
head. Run another `SessionStart` afterward. A corrupt/missing anchor or unsafe storage requires managed
decommissioning; the helper cannot infer a prior head from snapshot roots.
To replace a live anchor, stop managed hook use and sessions, durably call
`retire-installation` on the old anchor, retain its Retired tombstone, create
a different empty anchor/UUID, update trusted launchers and start new sessions.
If the old anchor is unavailable, disable its launchers and perform explicit
managed repair/decommissioning before replacement. Never copy Emitted heads
or treat a backup restore as a live relocation.

## Managed registrations

The examples use fixed absolute paths and a placeholder UUID. Install them
through protected client settings, not repository-owned hook configuration.
The Codex fragment belongs in a managed `requirements.toml` hook layer;
the Claude fragment belongs in managed `settings.json`. Replace the UUID with
the appropriate installation's generated value. Keep the handlers synchronous
and their timeout above the helper's 60-second deadline.

Codex managed TOML fragment:

```toml
[features]
hooks = true

[[hooks.SessionStart]]
matcher = "^(startup|resume|clear|compact)$"

[[hooks.SessionStart.hooks]]
type = "command"
command = "/opt/memory-hooks/bin/memory-hooks session-start --installation /var/lib/memory-hooks/codex-control --installation-id 11111111-1111-4111-8111-111111111111 --client codex-v1"
timeout = 90
async = false
additionalContextLimit = 0
```

Claude managed JSON fragment:

```json
{
  "hooks": {
    "SessionStart": [{
      "matcher": "^(startup|resume|clear|compact|fork)$",
      "hooks": [{
        "type": "command",
        "command": "/opt/memory-hooks/bin/memory-hooks session-start --installation /var/lib/memory-hooks/claude-control --installation-id 22222222-2222-4222-8222-222222222222 --client claude-v1",
        "timeout": 90,
        "async": false
      }]
    }]
  }
}
```

Codex [documents](https://learn.chatgpt.com/docs/hooks#sessionstart) the
four sources and the JSON output shape. Its
[large-output setting](https://learn.chatgpt.com/docs/hooks#large-hook-output)
uses zero to deliver complete additional context instead of a spill preview.
Claude [documents](https://code.claude.com/docs/en/hooks#sessionstart) `fork`
and optional top-level `agent_type`; this helper ignores that metadata but
rejects explicit delegated envelopes. Claude versions before 2.1.214 report
fork as `resume`. The local fixture suite exercises both adapters and sources;
deployment operators may separately smoke-test exact deployed client versions.

## Event and snapshot contract

The helper accepts exactly one UTF-8 JSON object, at most 64 KiB. Required
fields are `session_id` (1–1024 UTF-8 bytes), `cwd` (absolute, at most 4 KiB),
`hook_event_name = "SessionStart"`, and a supported `source`. Duplicate
required keys, wrong types, trailing JSON, malformed text, unsupported events
or sources, explicit delegation, and an event cwd unequal to the canonical
process cwd fail. Other bounded metadata cannot select project, profile,
context, config, endpoint or root. An unambiguous session ID claims a new
generation before source/cwd/config/root validation, retiring old success.

Schema v4 retains this exact root output contract. It also captures a bounded
private canonical process-cwd record tied to the root generation for
[direct-child publication](hooks-delegation.md). `SubagentStart` uses the
separate `delegated-start` command; it never reuses the root's output envelope.

The helper uses the shared five-second `HttpMemoryClient::guardrails` request
with the trusted binding context. It rejects redirects and ambient proxies.
The configured URL must be a root HTTP(S) origin, with no userinfo, query,
fragment or path prefix; root spellings normalize equivalently. No bootstrap,
capture or session-start remote write occurs on this mandatory path. The
shared pack limits remain 32 policies, 16 KiB canonical digest input, 24 KiB
serialized pack/publication and 32 KiB HTTP response. The hook event limit is
64 KiB, each private record is at most 128 KiB, and complete escaped JSON
output is bounded at 192 KiB; none is truncated.

The anchor holds the versioned installation manifest, activation journal,
session head/journal and fixed locks. The active snapshot root holds an exact
payload containing installation UUID, epoch/nonce, session hash, generation,
sequence, binding, complete pack and output hash/count. Pending retires the
previous head; Prepared means a payload is durable but output completion is
unproven; Emitted is committed only after all bytes are written and flushed;
Invalid remains unusable. The public reader starts at the fixed anchor and
requires current Active config, epoch/nonce, Emitted head, exact current-root
payload, binding, scope, pack/digest and output evidence. Timestamps and
matching digests alone are never authority.

Errors use bounded safe codes on stderr and a nonzero exit. They do not print
the bearer token, raw config/event, arbitrary path, Git stderr or upstream
body. A failed or partial write can leave Prepared; a crash after complete
output but before Emitted is a false negative requiring fresh delivery.
The fixed 60-second helper deadline bounds nonblocking input and output waits;
the shared guardrail HTTP request has its own five-second timeout. A broken
stdout pipe fails without Emitted evidence. If stdout stays blocked until the
helper deadline, partial bytes still leave the public reader unusable and the
next event must fetch afresh. Unfinished stdin fails before a session identity
can be trusted; it does not guess which old head to retire. Synchronous kernel
calls such as filesystem sync cannot be forcibly cancelled by the helper, so
an operating-system stall in one of those calls may exceed the deadline.
If the final directory sync for an Emitted or Active journal fails after its
rename, the helper rewrites that journal as unresolved while it still holds
the control locks. The caller receives an error; a newly opened public reader
rejects the transition until explicit repair and fresh delivery. If storage
also refuses the unresolved rewrite, the helper cannot establish durable
invalidation; stop use of the installation and perform managed repair or
decommissioning.
For a local process crash, the final atomic rename of the resolved session
journal is the commit point after complete output, payload persistence and
the Emitted head. A process killed after that rename but before the parent
directory sync leaves a coherent current Emitted snapshot readable by a newly
opened installation. Normal writer success still requires that directory sync.
A system crash or power loss in the same interval has weaker durability; the
process-kill result does not establish recovery after power loss.
Complete local output plus durable evidence is not client acknowledgement or
proof of obedience. Client switches may discard hook context, and #87 must
still revalidate actions. Filesystem durability is limited to the supported
local filesystem's ownership, `flock`, rename and `fsync` guarantees; hashes
and locks do not authenticate hostile same-UID writes or arbitrary rollback.

To exercise the repository fixtures, set `MEMORY_HOOKS_TEST_ROOT` to a private
supported filesystem (for example `/run/user/$(id -u)` on Linux). Set
`CARGO_TARGET_DIR` and `TMPDIR` to task-specific persistent paths under the
repository target directory, creating `TMPDIR` first. Run `cargo test -p
memory-hooks --all-features --test session_start`, `cargo test -p memory-hooks
--all-features --test snapshot`, and `cargo test -p memory-hooks --all-features
--test installation`. The integration tests start a local HTTP peer,
spawn the real helper, decode exactly one output JSON document, compare its
additional context byte for byte with the publication, and read the snapshot
through the public validator. They also check that a failed refresh cannot
reuse prior delivery. The fixtures cover all supported sources for both
adapters, repeated same-session starts with fresh generations and successor
revisions, general/project policy identities, selectors, values and exact
Unicode/escaped text, and representative response failures after earlier Emitted
delivery. Both subprocess adapters also exercise a valid 64-KiB event, a
1-KiB UTF-8 session ID, all 32 policies at a reachable shared byte ceiling,
maximum legal trusted context dimensions, and the exact 32-KiB HTTP body
limit. They reject ambiguous pre-identity input without guessing a session;
known-session event failures retire their old head before HTTP. The transport
fixtures reject declared or streamed body overflow, truncated responses with
and without a declared length, redirects, connection refusal, TCP reset and
disconnect. Both adapters expire stalled headers and bodies at the shared
five-second timeout, accept complete chunked responses, and reject unsafe
protected origins before HTTP. Child-only proxy variables cannot reroute the
bearer request. Private unit fault fixtures inject failures at temporary creation,
write, flush, file sync, rename and parent-directory sync for payloads,
session heads/journals and installation manifests/journals, including final
Emitted and Active journal resolution. They reopen the installation, test the
public reader, require explicit repair where authority is unresolved, and
prove old evidence remains unusable after a committed barrier. Run the
workspace test suite with `DATABASE_URL` pointing to a disposable migrated
PostgreSQL/pgvector database to exercise memoryd policy integration, including
no-policy and conflicting classified-policy responses through the daemon
endpoint, shared HTTP client, both adapter SessionStart workers and the public
reader. The daemon fixture covers scoped workstation/container contexts,
general and project override resolution, successor revisions and empty or
conflicting mandatory responses. Reopened-reader fixtures independently alter
payload scope, pack and output fields with a recomputed outer hash, and
head/journal fields; they also exercise checked epoch and sequence overflow.
Cross-process duplicate and separate-session starts show that a late fetch
cannot replace a newer claim or damage another session. Failed output on an
intermediate root cannot restore old R1 evidence after R1 → R2 → R1 migration.
The logical root-reversion fixture runs on one supported filesystem. An
additional fixture can exercise two distinct supported filesystems by setting
`MEMORY_HOOKS_TEST_ROOT_SECONDARY` to a second private fixture location; it
checks device identity before running. If no second location is configured,
the test reports that supplementary coverage as unavailable.
Output fixtures include interrupted
short writes, failure at the final byte/newline, a real broken stdout pipe,
a real blocked pipe until deadline, and unfinished input until deadline.
`cargo test -p memory-hooks --all-features --test snapshot` also kills a
separate test worker at Pending, binding attachment, Prepared before output,
partial output and complete output before Emitted. After each kill it reopens
the installation, rejects the interrupted attempt through the public reader,
and requires a fresh generation to recover. The worker barriers are confined
to the test binary; the production helper exposes no crash-stage switch.
The private replacement test binary also kills workers before and after
payload persistence, after the Emitted head sync while its journal is still
unresolved, and before final journal resolution. Kills immediately after the
final resolved-journal rename and after its directory sync preserve the exact
current snapshot. A returned final directory-sync error instead restores an
unresolved journal and requires repair and fresh delivery.
These fixtures establish the local protocol contract. Optional
real-client validation on deployed versions is an operator choice.

## Client compatibility evidence (2026-09-27)

An isolated Codex CLI 0.157.0 run loaded a protected `SessionStart` hook from
a disposable config home, invoked this helper against a local pack server,
and produced an Emitted anchor head. The model returned a marker present only
at the end of a shared-valid pack with roughly 14.4 KiB of rule text while
`additionalContextLimit = 0` was configured. A smaller exact-pack run also
returned its marker. This demonstrates complete context delivery on that
tested CLI path; it is not a general client acknowledgement protocol.
An isolated comparison without the override also returned the marker for this
repetitive fixture, so it did not establish the default spill boundary. The
documented default threshold and zero-limit behavior still require the
managed zero-limit setting above.

An isolated Claude Code 2.1.92 run invoked the helper and produced Emitted
local evidence. Its provider requests returned HTTP 401, so model consumption
could not be verified. The installed version also predates Claude's distinct
`fork` source (2.1.214); the fork adapter has subprocess fixture coverage but
has no verified distinct-source runtime consumption from that historical run.
The smoke used disposable roots and did not change live client settings or
policies. Authenticated external-provider smoke is optional operator
validation and does not gate deterministic fixture acceptance or implementer
ReadyForReview.
