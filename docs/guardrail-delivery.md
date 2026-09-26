# Guardrail delivery contract, version 1

`memory-mcp` requires an operator-selected `guardrails_project` and trusted
`resolution_context`. It fetches `GET /api/v1/projects/{project}/guardrails`
before opening stdio. The endpoint uses the authoritative policy resolver with
general policies included, safe contextual shadowing, and no tag filter.
Resolver conflicts and missing selector facts fail the whole request. An empty
mandatory set returns `guardrails_empty`; legacy contextual Rules are not
silently promoted. The endpoint sends `Cache-Control: no-store`.

Configure the execution target explicitly. For example, a host MCP session
can use `guardrails_project = "memory-server"` and
`resolution_context = { profile = "workstation-host", language = ["rust"] }`.
A CI container can use `profile = "woodpecker-container"` instead. The daemon
does not infer a profile from its own host, cwd, a tool argument, or policy
prose. If no bearer token is configured, deployments must rely on a trusted
network boundary for HTTP callers.

## Pack and digest

The version-one pack contains its selected project and context, resolver
schema version, ordered mandatory policies, digest algorithm, and digest.
Each policy carries source project, UUID, stable key, revision, class,
selectors, declared values, exact content, and override provenance. The
canonical digest input is compact UTF-8 JSON of a typed object with fields in
this order: `domain`, `resolver_schema_version`, `mandatory`. The domain is
`memory-guardrails-sha256-v1`. Nested selector sets and value maps use Rust
`BTreeSet` and `BTreeMap` order. Optional canonical rule fields serialize as
JSON `null`; omitted selector dimensions are absent from the selector object.
The digest is lowercase SHA-256 prefixed `sha256:`. Rule text is preserved as
a Rust string, including newlines and Unicode. Scope, timestamps, retrieval
results, and presentation text are outside the digest. Consumers must retain
both the scope and digest; identical policy sets can occur in different scopes.

The version-one golden vector in `memory-common/src/guardrails.rs` uses one
`general/a` revision with UUID `00000000-0000-0000-0000-00000000000a` and
content `Rüle "exact"` followed by a newline, tab, and `🦀`. Its digest is
`sha256:ea75e758b5fa19e4b8447119597a753ee4eb86dab0d8877fc13c0f7fadb27c67`.

Hard limits are 32 mandatory policies, 16 KiB of canonical input, 24 KiB each
for the serialized pack and rendered publication, 32 KiB of HTTP body, and
512 KiB for `tools/list`. Limits measure serialized UTF-8 bytes. An overflow
fails wholly with `guardrails_too_large` and policy identities. No mandatory
text is truncated or paginated. The HTTP client bounds the response and has a
five-second request timeout. Missing fields, unsupported versions, malformed
identities/order, scope mismatch, and digest mismatch fail closed.

## MCP visibility and lifetime

One connection publishes one immutable pack in initialization instructions,
every advertised tool description, and the read-only `memory_guardrails` tool.
Descriptions repeat exact mandatory text, identities, revisions, scope, and
digest for clients that hide instructions. Configure `memory_guardrails` as an
always-visible tool where the client supports that option. MCP cannot force a
client to surface a descriptor eagerly; a client hiding both instructions and
all descriptors has no automatic visibility guarantee.

Before `tools/list`, `memory_guardrails`, and every state-changing tool,
memory-mcp resolves the same scope again. A changed pack or any resolution or
transport failure permanently invalidates that connection. Guarded requests
then fail with a typed diagnostic and reconnect action. Policy writes through
that connection invalidate it after the successful write. New connections
fetch current policy. Instructions already delivered cannot be reliably
replaced in place across supported MCP versions; no `tools/list_changed`
notification is advertised.

Deploy a compatible daemon first, provision and verify applicable classified
mandatory policies under separate administrative authority, then deploy the
configured shim and reconnect clients. Old daemons and absent mandates fail
startup. This delivery contract cannot erase text retained by a disconnected
client or atomically lease policy across the daemon and a later mutation.
Downstream client enforcement and Cockpit snapshot propagation are separate
work.
