# Managed pre-tool guardrail gate

`memory-hooks pre-tool` is an opt-in synchronous gate for an observed Codex or
Claude Code `PreToolUse` event. It requires a protected [v3 configuration](../memory-hooks-v3.toml.example),
an active fixed installation anchor, and a fresh complete `SessionStart`
delivery for that exact client, session, repository, worktree and process cwd.
V1 inspection and v2 startup delivery remain supported, but a pre-tool call
under v2 is denied with `gate_contract_required`. Activating v3 advances the
installation epoch and cannot reuse a v2 Emitted snapshot.

For every observed tool call, including shell reads, file edits, MCP tools,
agent spawns, and unknown names, the gate claims the current generation before
fetching. It retrieves the same trusted project and normalized context through
the configured root origin, validates the new pack, and compares the complete
pack with the exact emitted payload. A matching check writes an anchor-relative
redacted audit record and returns exit 0 with empty stdout. The client then
runs its normal permission flow, which may still deny the tool. A changed pack,
failed fetch, invalid snapshot, conflicting cwd or audit failure denies this
invocation. The hook never refreshes or replays the proposed tool call. The
agent may replan only after a new successful `SessionStart` delivery.

## Registration

Provision one protected anchor per client with `init-installation`, retain its
generated UUID, and activate v3 as described in [delivery operations](hooks-delivery.md).
The command, installation path, UUID and adapter are operator-managed settings,
not values from hook input. Use a catch-all matcher and synchronous execution.
Do not set an `if` filter. The 90-second client timeout exceeds the gate's
35-second worker deadline and leaves time for retirement, audit and denial.

Codex `config.toml` example, added alongside its existing `SessionStart`
registration:

```toml
[[hooks.PreToolUse]]
matcher = "*"

[[hooks.PreToolUse.hooks]]
type = "command"
command = "/opt/memory-hooks/bin/memory-hooks pre-tool --installation /var/lib/memory-hooks/codex-control --installation-id 11111111-1111-4111-8111-111111111111 --client codex-v1"
async = false
timeout = 90
```

Claude Code `settings.json` example, added alongside its existing
`SessionStart` registration:

```json
{
  "hooks": {
    "PreToolUse": [{
      "matcher": "*",
      "hooks": [{
        "type": "command",
        "command": "/opt/memory-hooks/bin/memory-hooks pre-tool --installation /var/lib/memory-hooks/claude-control --installation-id 22222222-2222-4222-8222-222222222222 --client claude-v1",
        "async": false,
        "timeout": 90
      }]
    }]
  }
}
```

These are managed registration examples, not an installation action. Verify
that the chosen client actually runs the handler synchronously for every
covered tool path. Operator configuration declares the intended capability;
it cannot attest at runtime that a hook was installed. The same binary rejects
unrecognized event shapes. In schema v4, a verified Claude child carries a
separate `agent_id` and is checked against its own published snapshot and
current parent; older configurations still deny child mutations. See
[managed delegated hooks](hooks-delegation.md) for its exact pin and limits.

## Denial and recovery

Normal denial is a bounded `hookSpecificOutput` JSON document with
`permissionDecision: "deny"` and a stable reason code. If that document cannot
be emitted, the command exits 2 with a short static stderr message. Neither
path grants permission. The codes include `fresh_session_required`,
`policy_changed`, `guardrails_unavailable`, `snapshot_invalid`,
`scope_mismatch`, `event_invalid`, `gate_contract_required`,
`unsupported_capability` and `audit_failed`. Schema v4 also distinguishes
`missing_child_linkage` and `parent_invalid`. A failed current check leaves a
permanent generation tombstone. Restore service or configuration, then deliver
a fresh `SessionStart` and replan the denied action. Repeating the same old
invocation never converts its denial into permission.
For an invalid v4 child, start a new child ID after the parent is freshly
delivered; repeating `SubagentStart` for the old ID cannot replace its context.

An audit record contains only its schema version, random event and attempt
UUIDs, adapter, fixed tool category, hashed session and binding identities,
validated epoch and generation, intended outcome and allowlisted reason. It
does not contain commands, patches, tool input, paths, URLs, names, credentials
or policy text. Neutral means only that this hook raised no objection; it does
not prove native permission approval or tool execution. The v3 audit contract
uses the installation anchor, with at most 4,096 records, 8 MiB total and
2 KiB per record by default. Capacity exhaustion denies guarded events.
Export and retain the protected anchor records under your operator retention
policy. To rotate a full store, retire that installation, provision a new
anchor/UUID, activate the protected configuration and deliver new sessions;
do not delete individual records while an installation is active. No automatic
deletion or upload occurs.

## Capability boundary

The gate covers supported local PreToolUse callbacks. A hosted tool, a client
that skips the hook, an async registration, or a client that ignores a blocking
result is outside its enforcement. Codex later `write_stdin` interactions with
an already-running shell are outside this interception point. The gate checks
structured execution cwd/workdir when supplied, but does not parse shell `cd`,
infer remote MCP execution targets, inspect every file a tool may touch, or
apply semantic shell/Rust command policy. Same-UID tampering with the helper,
its process or protected files is outside the private-storage threat model.
The final locked local decision orders checks against invalidation. A later
daemon policy change or SessionStart cannot retroactively revoke a tool call
already handed back to the client; there is no transaction across daemon state,
hook exit and actual execution.

The old `hooks/pre-command.sh` now returns a static exit-2 migration denial
and no longer forwards raw command or metadata to remote session logs. Remove
its old PreToolUse registration before using the managed gate. Existing
historical raw logs are not automatically scrubbed; other independently
configured transcript capture is outside this gate's redaction guarantee.

Protocol fixtures follow the documented [Codex PreToolUse contract](https://learn.chatgpt.com/docs/hooks#pretooluse)
and [Claude PreToolUse contract](https://code.claude.com/docs/en/hooks#pretooluse-decision-control),
read 2026-09-28. Local binaries reported Codex CLI 0.158.0 and Claude Code
2.1.92 during implementation; those version strings and startup observations
are not live PreToolUse interoperability evidence.
