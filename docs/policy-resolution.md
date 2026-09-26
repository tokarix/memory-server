# Policy resolution

Rules and bootstrap use the same deterministic resolver. A request reads all
Rule candidates for the requested project and `general` in one database
statement. Superseded revisions remain inspectable through ordinary get/list
and search operations, but never become a fallback when a successor has a
different scope. Legacy Rules remain contextual and additive by UUID.

## Context and trust

The `profile` identifies where the work executes. `workstation-host` and
`woodpecker-container` are distinct asserted targets. The daemon does not
derive a profile from its own host, cwd, tags, or policy prose. Direct HTTP
resolution trusts the authenticated caller's `context` query assertion; this
does not attest a machine or add multi-tenant authentication. MCP binds the
context from operator-supplied startup configuration. Model tool arguments
may only repeat configured dimensions exactly, and cannot fill an unknown
dimension. Hooks read `MEMORY_RESOLUTION_CONTEXT` JSON from the operator's
environment and URL-encode it; captured model or user text has no role.

`profile` and `phase` are optional scalar identifiers. `language` and `tool`
are optional sets. Missing means unknown; `[]` means known none. A policy
language/tool dimension matches on set intersection. A known mismatch in any
dimension excludes that policy even if another dimension is unknown. A
remaining unknown dimension fails with `policy_context_required` and lists
both missing fields and affected policies. This check precedes contextual
tag filtering. Excluded optional general contextual policies do not demand
context.

## Identity, containment, and conflicts

One active revision exists per `(project, policy_key)`. A successor globally
replaces its predecessor, including its selector domain. Environment variants
use distinct stable keys. For example, `rust.build.storage.workstation` may
select `profile=["workstation-host"]`, while
`rust.build.storage.woodpecker` selects
`profile=["woodpecker-container"]`. Both can declare a value for
`rust.build.target_storage` because their domains do not intersect.

One selector domain is narrower than another when every dimension denotes a
subset of the corresponding dimension. An absent dimension is universal.
`profile=["workstation-host"], language=["rust"]` is narrower than
`language=["rust"]`; `profile=["workstation-host"]` and
`language=["rust"]` alone are incomparable. Applicable same-key project
policies may replace contextual general policies only when project selectors
are equal or narrower. `shadow_general=false` rejects such a collision.
Project policies cannot replace an applicable mandatory general policy.

After overrides, all effective structured policies must agree on every shared
declared setting. `rust.build.target_storage=persistent-disk` and
`rust.build.target_storage=container-local-tmp` conflict when both apply,
even if one policy's tags would filter it from the response. Identical
assignments coexist. This exact-value contract detects declared settings;
arbitrary prose contradictions are not inferred.

Mandatory winners are always returned. Tags match ALL-of for contextual and
legacy Rules only. `include_general=false` removes optional general guidance
but never applicable mandatory general policies. A replaced general policy
does not reappear when its project winner lacks a requested tag. Duplicate
active identities fail instead of choosing a revision by time or specificity.

Failures have stable `policy_*` codes, normalized context, sorted policy
references, and a corrective action. Malformed or incomplete context uses
HTTP 400. Policy set conflicts use HTTP 409. Rules and bootstrap return no
partial success. The HTTP client preserves the envelope through MCP
`ErrorData.data`.

## Canonical output and rollout

Both endpoints return separate `general_rules` and `project_rules` arrays,
plus `canonical` schema version 1. `canonical.effective` includes all returned
Rules in a total order: mandatory first, then key, source project, revision,
UUID; legacy contextual Rules follow by UUID. `canonical.mandatory` is the
mandatory-only ordered projection used by guardrail delivery. Each entry
contains source project, UUID, key/revision/class when classified, selectors,
declared values, exact content, and replacement provenance. It excludes
timestamps, embeddings, similarity, and recall. The separate response
`context` and `options` record trusted execution facts and request choices.
The strict `/api/v1/projects/{project}/guardrails` endpoint resolves with
`include_general=true`, `shadow_general=true`, and no tag filter, then
publishes only `canonical.mandatory`. It fails on an empty or excessive pack.
See [guardrail delivery](guardrail-delivery.md) for the digest and MCP
connection contract. Delivery does not enforce downstream client behavior.

Deploy compatible daemon, MCP shim, and hooks before adopting scoped data.
Old unscoped requests continue to work. Once scoped data exists, requests
missing a required context fail visibly. The additive scope migration can
roll back only while every stored selectors and values object is empty; its
down migration refuses any data loss. The earlier identity migration's
classified-history rollback refusal remains in force. Preserve source Rule
history through explicit adoption and successors when converting existing
guidance; never edit historical classified bodies in place.
