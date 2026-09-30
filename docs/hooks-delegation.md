# Managed delegated hooks

Schema v4 adds an opt-in, direct-child contract to the existing protected
installation. `memory-hooks delegated-start` reads one `SubagentStart` event
through the same pinned installation path, UUID, and adapter arguments as
`session-start`. A root `Emitted` snapshot never authorizes a child. In enforced
mode, the child has its own stable key, generation, complete pack, exact
`SubagentStart` output evidence, guard, and recorded parent generation and
binding. The catch-all `pre-tool` command checks that child record and the
current parent before it returns an empty neutral response.

| Exact client candidate | Context delivery | Child tool identity and denial | Covered spawn | Mode |
| --- | --- | --- | --- | --- |
| Claude Code 2.1.92 | Direct same-cwd `SubagentStart` context observed in a disposable live-client fixture | `agent_id` on child `PreToolUse`; blocking denial observed | Parent `Agent` callback observed | `enforced` or `advisory` |
| Codex CLI 0.158.0 | Documented `SubagentStart` context; live child consumption unverified | Documented `PreToolUse` does not establish child linkage | Unverified | `advisory` only |

These facts are specific to the selected versions and paths. Configuration
records the intended pin; it cannot prove that the installed client or hook
registration matches it. A real Claude Code 2.1.92 fixture against a local
scripted API observed the parent `Agent` callback, child `SubagentStart`, and
child `Bash` callback with the same child ID and enclosing session ID. The
injected context reached the child's next API request, and a hook denial
prevented a pending marker mutation. The fixture used a direct child in one
cwd; it did not test nested agents, another project, repeat-start replacement,
or every tool implementation. Current [Claude hook documentation](https://code.claude.com/docs/en/hooks#subagentstart)
describes the same identifiers and warns that a repeated start can retain the
earlier context. Current [Codex hook documentation](https://learn.chatgpt.com/docs/hooks#subagentstart)
does not establish child linkage on guarded tool callbacks.

The actual `memory-hooks` binary was also exercised with Claude Code 2.1.92
against disposable local Claude and guardrail APIs. The root start, parent
`Agent` check, and child start used the same valid pack; a changed fourth
guardrail response at the child's `Bash` check produced a redacted
`policy_changed` audit record. The child request contained the guardrail text,
and its pending `marker.txt` mutation was absent after the run. Since the root
received the same text, that request alone cannot distinguish root text from
child injection; the separate distinct-marker fixture above establishes
`SubagentStart` consumption. Together these runs cover the pinned same-cwd
helper and client path within the local mock's tool sequence and registration.

The [v4 example](../memory-hooks-v4.toml.example) selects the Claude pin and
`mode = "enforced"`. Activation advances the epoch and requires a new root
`SessionStart` delivery. It does not import v3 session records as child
evidence. `mode = "advisory"` accepts a typed child start but emits no guardrail
context and never authorizes child tools. An observed unsupported Codex child
marks the enclosing session ambiguous: root-shaped guarded callbacks then
deny, and ordinary same-session refresh does not clear the marker. Start a
new managed session to recover.

For a managed Claude registration, use the same protected path and UUID in
all three synchronous command handlers. Keep the `PreToolUse` matcher catch-all
so edits, shell, MCP, unknown tools, and parent spawn are observed. This is a
registration example; the helper does not install it:

```json
{
  "hooks": {
    "SessionStart": [{"hooks": [{"type": "command", "command": "/opt/memory-hooks/bin/memory-hooks session-start --installation /var/lib/memory-hooks/claude-control --installation-id 22222222-2222-4222-8222-222222222222 --client claude-v1", "async": false, "timeout": 90}]}],
    "SubagentStart": [{"hooks": [{"type": "command", "command": "/opt/memory-hooks/bin/memory-hooks delegated-start --installation /var/lib/memory-hooks/claude-control --installation-id 22222222-2222-4222-8222-222222222222 --client claude-v1", "async": false, "timeout": 90}]}],
    "PreToolUse": [{"matcher": "*", "hooks": [{"type": "command", "command": "/opt/memory-hooks/bin/memory-hooks pre-tool --installation /var/lib/memory-hooks/claude-control --installation-id 22222222-2222-4222-8222-222222222222 --client claude-v1", "async": false, "timeout": 90}]}]
  }
}
```

The child lifecycle checks the event cwd against the actual hook process cwd,
then reads the parent's protected cwd record, current binding, idle or
completed guard, complete emitted pack, and exact generation. It fetches the
parent's trusted project and context again and compares the entire pack. A
changed pack or failed fetch conditionally retires only the captured parent
generation. The child independently resolves its canonical binding. The
verified contract currently permits only the same cwd and scope as the parent;
it does not relabel the parent's pack for another project. A target-project or
different-cwd child start emits no authoritative context. A later child tool
callback in another cwd is denied; replan in a newly managed, independently
bound session rather than replaying that pending mutation.

The enforced Claude path limits the child's publication to 10,000 UTF-8
bytes, below the client's documented inline-context cap. A larger shared-valid
pack is rejected for child publication because a file path and preview cannot
prove full child context delivery. Keep the registered command timeout above
the helper's own deadline; a client-canceled command hook can proceed through
normal permission flow without its decision.

Each child publication attempts `Pending` then `Prepared`, writes and flushes
the exact `hookSpecificOutput` with `hookEventName = "SubagentStart"`, stores
required redacted audit evidence, and only then commits `Emitted`. A partial
output, crash, failed audit, or missing parent linkage cannot authorize a
mutation. For each observed child tool call, the gate claims only that child's
guard, re-fetches the shared scope once for both child and parent, compares
the complete pack, and finishes under installation and ordered parent/child
locks. Parent refresh, activation, scope change, or another child check can
still deny. A sibling does not borrow the parent's or another sibling's
success. Neutral output only means this helper raised no objection; native
client permissions still decide whether the tool runs.

A repeated `SubagentStart` with the same child ID is tombstoned and emits no
new context, even when its rules appear unchanged. The client may retain its
original context across a resume or compaction, so changed parent rules or
an invalid child require a new child ID with verified fresh delivery. Nested
delegation is unsupported because the pinned event does not prove the
immediate parent identity; observed child `Agent` calls deny. Root `Agent`
calls pass the ordinary fresh gate before native spawn permission, while
other observed agent tool names deny in enforced mode. A valid parent spawn
check is not evidence that the future child start hook ran.

The helper validates required fields, types, duplicate keys, UTF-8, trailing
JSON, control characters, bounds, event names, and actual cwd. `agent_type`,
turn IDs, transcript paths, prompts, and tool arguments never select
authority. Private records and audit contain hashed identities and fixed
reason categories, not those freeform values or credentials. See
[root delivery](hooks-delivery.md) and [the generic gate](hooks-gate.md) for
their existing limits and recovery rules.

This covers observed, synchronous local hooks only. Hosted tools, remote
execution, long-lived shell input such as `write_stdin`, skipped or untrusted
hooks, async hooks, and client kills are outside this boundary. Same-UID
tampering is also outside the protected installation model. A nonblocking
`SubagentStart` cannot prevent child creation, and local emission proves
neither model obedience nor a sandbox or semantic policy interpretation.
