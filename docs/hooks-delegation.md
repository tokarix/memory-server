# Delegated hook status

`memory-hooks delegated-start` accepts a bounded, typed `SubagentStart` event
through the same protected installation path, UUID, and client adapter arguments
as `session-start`. In the current root-only configuration it writes
`advisory_only` to safe stderr, emits no context, and exits successfully because
the client startup event cannot block child creation. This outcome is not
guardrail delivery. Where the v3 catch-all gate is registered, a recognized
child `PreToolUse` carrying `agent_id` is denied; it cannot use the enclosing
root session's snapshot.

The exact local binary versions available during this implementation were
Codex CLI 0.158.0 and Claude Code 2.1.92. Both remain advisory in this helper:

| Candidate | Documented child identity | Pinned live evidence | Current status |
| --- | --- | --- | --- |
| Codex CLI 0.158.0 | `SubagentStart` has `agent_id`; documented `PreToolUse` lacks child linkage | No successful child smoke | Advisory only |
| Claude Code 2.1.92 | `agent_id` on `SubagentStart` and child `PreToolUse` | Direct, same-cwd loopback fixture passed | Advisory only until child state is implemented |

For Claude Code 2.1.92, a disposable run against a local scripted API observed
the parent `Agent` pre-tool callback, then `SubagentStart`, then the child's
`Bash` pre-tool callback. The latter two carried the same child ID and enclosing
session ID; hook cwd matched the process cwd. A token emitted as
`SubagentStart.additionalContext` appeared in the subsequent child API request.
A hook denial appeared in the child's next request, and the requested marker
file was absent. The fixture used a direct child in one cwd and did not test
cross-project execution, nested delegation, repeated start, or helper snapshot
integration. The ordinary remote model endpoint timed out, so this evidence
comes from the real pinned client driven by a local deterministic response.

The helper validates `session_id`, `agent_id`, event name, adapter shape, and
actual process cwd without using agent type, transcript path, prompt, or tool
arguments to choose authority. A parent's `Emitted` state proves only its own
local publication. A child needs separate durable publication, scope, and
parent lineage before any covered mutation can become neutral. That state
machine is not enabled by the advisory command.

Current [Codex hook documentation](https://learn.chatgpt.com/docs/hooks#subagentstart)
describes context injection at `SubagentStart`, but the startup callback cannot
prevent creation. The [Claude hook documentation](https://code.claude.com/docs/en/hooks#subagentstart)
also says repeated startup callbacks can retain the earlier injected context.
Therefore a later output cannot by itself prove that a running child received
replacement rules. These current documents are protocol references, not live
evidence for the versions above.

Do not register the advisory command as a guardrail delivery mechanism. Missing
child startup, nested delegation without immediate-parent linkage, remote or
hosted execution, long-lived shell operations such as `write_stdin`, skipped or
untrusted hooks, asynchronous hooks, and client termination remain outside
the helper's demonstrated coverage. Same-UID tampering with client or helper
processes is also outside the installation's trust boundary. Local emission is
not a sandbox or semantic policy enforcement.
