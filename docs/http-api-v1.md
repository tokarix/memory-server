# memoryd HTTP API v1

This document defines the proposed split of the current `memory-server`
into:

- `memoryd`: long-running HTTP service with database, embedding, search,
  reranking, and session-log storage
- `memory-mcp`: small stdio MCP adapter that translates MCP tool calls
  into HTTP requests to `memoryd`

The first HTTP surface uses a versioned REST-style API under
`/api/v1`, with OpenAPI generated from Rust types and served through
Scalar.

## Goals

- Keep the MCP-facing binary small and stateless
- Move database and Ollama dependencies to a single remote service
- Support exact session logging of every prompt/response/tool event
- Preserve the existing semantic memory workflows
- Make the API stable enough for a web UI, admin CLI, and future team use

## Non-goals

- Public multi-tenant SaaS concerns in v1
- Streaming generation over HTTP in v1
- Replacing MCP with HTTP at the agent boundary

## Components

### `memoryd`

Responsibilities:

- Own PostgreSQL connection pooling and migrations
- Call Ollama or future embedding/rerank providers
- Store and search durable memories
- Record raw session logs and chunk embeddings
- Expose OpenAPI and Scalar docs
- Enforce auth and request validation

Recommended Rust stack:

- `axum`
- `utoipa`
- `utoipa-scalar`
- `sqlx`
- `pgvector`
- `tower-http`
- `tracing`

### `memory-mcp`

Responsibilities:

- Speak MCP over stdio
- Validate MCP tool parameters
- Translate tool calls to HTTP requests
- Map HTTP errors into MCP errors
- Hold no database or embedding logic

## URL layout

Base prefix:

```text
/api/v1
```

Primary endpoints:

- `GET /api/v1/health`
- `GET /api/v1/openapi.json`
- `GET /scalar`
- `POST /api/v1/memories`
- `POST /api/v1/memories/search`
- `GET /api/v1/memories/{id}`
- `PATCH /api/v1/memories/{id}`
- `DELETE /api/v1/memories/{id}`
- `GET /api/v1/projects/{project}/recall`
- `GET /api/v1/projects/{project}/rules`
- `POST /api/v1/sessions`
- `POST /api/v1/sessions/start`
- `POST /api/v1/sessions/{id}/messages`
- `POST /api/v1/sessions/{id}/finalize`
- `GET /api/v1/projects/{project}/review-queue`
- `POST /api/v1/review`
- `GET /api/v1/memories/{id}/neighbors`
- `POST /api/v1/session-search`

Notes:

- Search stays `POST` because it will grow request bodies beyond clean
  query-string usage.
- Scalar is served at `/scalar`, while the OpenAPI document is served at
  `/api/v1/openapi.json`.

## OpenAPI and Scalar

Generate the OpenAPI document from handler DTOs using `utoipa`.

Recommended setup:

```rust
#[derive(OpenApi)]
#[openapi(
    paths(
        health,
        create_memory,
        search_memories,
        get_memory,
        update_memory,
        delete_memory,
        recall_project,
        list_project_rules,
        create_session,
        append_session_message,
        finalize_session,
        search_sessions,
    ),
    components(
        schemas(
            ApiError,
            CategoryDto,
            CreateMemoryRequest,
            FinalizeSessionRequest,
            MemoryDto,
            MemorySearchRequest,
            MemorySearchResponse,
            RuleListResponse,
            SessionDto,
            SessionMessageDto,
            SessionSearchRequest,
            SessionSearchResponse,
            UpdateMemoryRequest,
        )
    ),
    tags(
        (name = "health"),
        (name = "memories"),
        (name = "projects"),
        (name = "sessions"),
    )
)]
pub struct ApiDoc;
```

Serve Scalar from the generated spec:

```rust
Router::new()
    .merge(Scalar::with_url("/scalar", ApiDoc::openapi()))
    .route(
        "/api/v1/openapi.json",
        get(|| async { Json(ApiDoc::openapi()) }),
    )
```

## Authentication

Use bearer-token auth in v1.

Recommended behavior:

- Read a static token from config or env
- Require `Authorization: Bearer <token>` on all `/api/v1/*` endpoints
  except `/api/v1/health`
- Support reverse-proxy TLS termination outside the app

OpenAPI:

- Define one bearer auth scheme
- Apply it globally except for `health`

## Classified Rule metadata

`POST /api/v1/memories` accepts optional `policy` only for category `rule`.
The write object requires `policy_key`, positive signed 64-bit `revision`, and
`delivery_class` (`contextual` or `mandatory`); it may include the active
predecessor UUID as `supersedes`. Clients cannot set `state`. The read object
adds `state` (`active` or `superseded`). A missing or null `policy` means an
unclassified legacy Rule or an ordinary non-Rule memory. There is no automatic
backfill or inference from content, summaries, tags, or timestamps.

The identity is `(project, policy_key)`. A first revision may use any positive
number and omits `supersedes`. Every later revision uses a larger number and
names the current active predecessor. General and project Rules with the same
key are independent. Classified revisions cannot be edited or deleted through
generic memory endpoints; publish a successor instead. Superseded Rules are
omitted from Rule lists, bootstrap, and direct core recall, but remain available
through direct get, ordinary lists, search, and neighbors. Applicable mandatory
Rules bypass tag and optional-general filters during resolution.

Optional `selectors` contains `profile`, `phase`, `language`, and `tool`
dimensions. Each present dimension is a nonempty array of at most 32 canonical
identifiers. Members are OR; dimensions are AND. Missing dimensions are
universal. Optional `values` maps at most 64 canonical setting keys to exact,
nonblank strings of at most 1024 UTF-8 bytes. Conflicting values for one key
fail resolution, including across distinct policy keys. Neither selectors nor
values are inferred from tags or policy prose.

`PATCH /api/v1/memories/{id}` also accepts a metadata-only adoption request
for a legacy Rule. Read the Rule first, inspect its contents, then send only
`policy` and `expected_updated_at`. The latter must be a timezone-qualified
RFC3339 string copied exactly from the MCP `memory_get` metadata header (or
the persisted `updated_at` value in the HTTP GET response). Example:

```json
{
  "expected_updated_at": "2026-09-26T12:34:27.123456000Z",
  "policy": {
    "policy_key": "build.storage",
    "revision": 1,
    "delivery_class": "contextual"
  }
}
```

Replace the example timestamp with the actual value read for that UUID.
Adoption preserves the UUID, text, tags, embedding, and creation time. An
intervening edit produces HTTP 409 `policy_stale_assignment`; inspect the
changed Rule before retrying. Missing, malformed, or zoneless tokens produce
HTTP 400. The old human-readable minute display is not a token. Policy
validation and immutable-revision errors use a stable `policy_*` code and
structured `details` in the error envelope; the MCP bridge preserves both.

The reversible SQL migration has an intentionally conservative down path:
it refuses to run once any structured policy metadata exists. Stop old
write-capable daemon instances before publishing policies, and use a separately
authorized export or restore if classified history must be removed.

## Error model

Use a consistent JSON error envelope.

```json
{
  "error": {
    "code": "memory_not_found",
    "message": "Memory 550e8400-e29b-41d4-a716-446655440000 not found",
    "details": null,
    "request_id": "req_01h..."
  }
}
```

Suggested fields:

- `code`: stable machine-readable identifier
- `message`: human-readable message
- `details`: optional structured payload
- `request_id`: correlation ID for logs

Suggested common codes:

- `unauthorized`
- `validation_error`
- `memory_not_found`
- `session_not_found`
- `embedding_error`
- `database_error`
- `internal_error`

## Data model

The existing `memories`, `session_logs`, and `session_log_chunks` tables
should evolve into a more normalized session model.

### Durable memory tables

Keep the core memory concept:

- `memories`
- `memory_chunks` optional later, if full-content chunk search becomes necessary

`memories` should continue to hold:

- `id`
- `project`
- `category`
- `summary`
- `content`
- `tags`
- `embedding`
- `created_at`
- `updated_at`

### Session logging tables

Proposed v1 session schema:

#### `sessions`

- `id UUID PRIMARY KEY`
- `external_session_id TEXT UNIQUE NOT NULL`
- `project TEXT NOT NULL`
- `cwd TEXT NOT NULL`
- `agent TEXT NULL`
- `model TEXT NULL`
- `summary TEXT NOT NULL DEFAULT ''`
- `started_at TIMESTAMPTZ NOT NULL`
- `ended_at TIMESTAMPTZ NULL`
- `created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`
- `updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`

#### `session_messages`

- `id UUID PRIMARY KEY`
- `session_id UUID NOT NULL REFERENCES sessions(id) ON DELETE CASCADE`
- `seq INTEGER NOT NULL`
- `role TEXT NOT NULL`
- `kind TEXT NOT NULL`
- `content TEXT NOT NULL`
- `tool_name TEXT NULL`
- `created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`

Constraints:

- `UNIQUE(session_id, seq)`

`kind` should distinguish:

- `user`
- `assistant`
- `tool_call`
- `tool_result`
- `system`

#### `session_chunks`

- `id UUID PRIMARY KEY`
- `session_id UUID NOT NULL REFERENCES sessions(id) ON DELETE CASCADE`
- `message_id UUID NULL REFERENCES session_messages(id) ON DELETE CASCADE`
- `chunk_index INTEGER NOT NULL`
- `content TEXT NOT NULL`
- `embedding VECTOR(1024) NOT NULL`
- `created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`

Indexes:

- B-tree on `sessions(project, started_at DESC)`
- B-tree on `session_messages(session_id, seq)`
- HNSW on `session_chunks(embedding vector_cosine_ops)`

This design gives:

- exact replay of the session
- semantic search over chunked content
- future summarization into curated memories

## DTOs

These DTOs should be separate from `sqlx` row structs.

### Memory DTOs

```json
{
  "id": "uuid",
  "project": "ubus-rs",
  "category": "decision",
  "summary": "Use a standalone cleanroom Rust workspace",
  "content": "Long-form memory body",
  "tags": ["rust", "cleanroom"],
  "created_at": "2026-03-09T12:00:00Z",
  "updated_at": "2026-03-09T12:30:00Z"
}
```

`CreateMemoryRequest`:

- `project`
- `category`
- `summary`
- `content`
- `tags` optional

`UpdateMemoryRequest`:

- `summary` optional
- `content` optional
- `tags` optional

`MemorySearchRequest`:

- `project`
- `query`
- `category` optional
- `limit` optional
- `min_similarity` optional

`MemorySearchResponse`:

- `results: Vec<MemorySearchHit>`

`MemorySearchHit`:

- `memory: MemoryDto`
- `score: f64`
- `source: "memory"`

### Session DTOs

`CreateSessionRequest`:

- `external_session_id`
- `project`
- `cwd`
- `agent` optional
- `model` optional
- `started_at` optional

`AppendSessionMessageRequest`:

- `seq`
- `role`
- `kind`
- `content`
- `tool_name` optional
- `created_at` optional

`FinalizeSessionRequest`:

- `summary`
- `ended_at` optional
- `embed_chunks` default `true`

`SessionSearchRequest`:

- `project`
- `query`
- `limit` optional
- `min_similarity` optional

`SessionSearchResponse`:

- `results: Vec<SessionSearchHit>`

`SessionSearchHit`:

- `session_id`
- `external_session_id`
- `summary`
- `chunk_excerpt`
- `score`

## Endpoint semantics

### `GET /api/v1/health`

Purpose:

- liveness/readiness probe

Response:

```json
{
  "status": "ok",
  "version": "0.2.0",
  "git_hash": "abcdef1"
}
```

### `POST /api/v1/memories`

Purpose:

- create a new curated memory

Behavior:

- generate embedding from summary + content server-side
- store a single memory row

### `POST /api/v1/memories/search`

Purpose:

- semantic/hybrid search over curated memories

Behavior:

- expand the query only when requested and configured
- embed the query server-side
- run vector + FTS hybrid search
- apply rerank only when requested and configured
- exclude task-scoped workflow artifacts by default before every vector,
  full-text, graph-neighbor, and session-log candidate limit
- include those artifacts when the optional JSON field
  `include_workflow_artifacts` is `true`

Workflow artifacts are retained records: task-scoped Plans, recursively linked
superseded Plan revisions, Decisions that review those Plans, and correlated
raw or normalized task session transcripts. Exact list/get, public-neighbor,
review, normalized-session, and session-log browse endpoints remain inclusive.

### `GET /api/v1/memories/{id}`

Purpose:

- retrieve one memory

### `PATCH /api/v1/memories/{id}`

Purpose:

- partial update

Behavior:

- if summary or content changes, fetch current record and re-embed from
  effective full text, not just the partial patch

### `DELETE /api/v1/memories/{id}`

Purpose:

- hard-delete one memory

### `GET /api/v1/projects/{project}/recall`

Purpose:

- return project memories considered core recall

Query parameters:

- `include_workflow_artifacts=true` optional, default `false`

Behavior:

- mirrors current `memory_recall`
- exclude task-scoped workflow artifacts unless explicitly included

### `GET /api/v1/projects/{project}/rules`

Purpose:

- return all `rule` memories for the given project

Query parameters:

- `include_general=true` optional, default `true`
- `tags` optional, comma-separated; ALL-of filter for contextual Rules only
- `shadow_general` optional, default `true`
- `context` optional, URL-encoded JSON object with `profile` and `phase`
  scalars and `language` and `tool` arrays

Behavior:

- include project-specific rules
- always include applicable mandatory general policies, even when
  `include_general=false`; optional general guidance obeys the flag
- project policies replace same-key contextual general policies only when
  their selector domain is equal or narrower and `shadow_general=true`
- a same-key collision with mandatory general policy is HTTP 409, regardless
  of tags, `include_general`, or `shadow_general`
- unknown required context is HTTP 400; incompatible values or ambiguous
  overrides are HTTP 409, with sorted references and corrective action
- return stable `general_rules`, `project_rules`, `context`, and a versioned
  `canonical` representation with `effective` and `mandatory` projections

### `GET /api/v1/projects/{project}/bootstrap`

Purpose:

- load the effective rules needed to start or resume a session

Query parameters:

- `include_general=true` optional, default `true`
- `include_recall=true` optional, default `true`
- `context` optional, with the same typed JSON contract as rules

Behavior:

- return `general_rules`
- return `project_rules`
- return non-rule `recall_memories` for the project
- resolve the complete policy set before recall; conflict responses contain
  no partial bootstrap or recall payload
- omit every Plan and review Decision from `recall_memories`, independent of
  the standalone recall opt-in
- the review exclusion applies to Decisions tagged `review`; other categories
  carrying that tag are now eligible, unlike the previous broader filter
- this is the preferred endpoint for session-start and first-prompt hooks

### `POST /api/v1/sessions`

Purpose:

- compatibility endpoint that stores a whole session log snapshot

Behavior:

- preserves the existing monolith/session-log ingestion path

### `POST /api/v1/sessions/start`

Purpose:

- create or upsert a logical session row before messages arrive

Behavior:

- upsert by `external_session_id`

### `POST /api/v1/sessions/{id}/messages`

Purpose:

- append one message/event to a session

Behavior:

- write append-only `session_messages` row
- no embedding yet
- include `agent`, `role`, optional `kind`, and optional `metadata`

### `POST /api/v1/sessions/{id}/finalize`

Purpose:

- compute session summary and searchable chunks

Behavior:

- mark `ended_at`
- aggregate ordered `session_messages` into session text
- chunk aggregated session text
- embed chunks
- refresh `session_log_chunks`

This endpoint is a better fit for hooks than trying to store curated
memories automatically.

### `GET /api/v1/projects/{project}/review-queue`

Purpose:

- list memories tagged `review-needed`, with optional category filter

Behavior:

- supports `category` and `limit` query parameters
- when `category` is provided, only returns memories of that category
- intended for cross-agent review handoff (plan reviews, code reviews)

### `POST /api/v1/review`

Purpose:

- store a review decision for a memory and mark it reviewed

Request body:

- `memory_id`: UUID of the memory to review
- `reviewer`: reviewer identity
- `verdict`: e.g. `approved`, `changes-requested`, `rejected`
- `notes`: review notes
- `project`: optional override project

Behavior:

- stores a `decision` memory linked to the original memory
- updates the original memory tags by removing `review-needed`
- adds reviewer/verdict tags for later auditing

### `GET /api/v1/memories/{id}/neighbors`

Purpose:

- list neighbor memories reachable via graph edges

Query parameters:

- `limit` optional, default 20

Behavior:

- returns non-suppressed edges and the connected memory summaries
- follows edges in both directions (src or dst matches the given ID)
- results ordered by edge weight descending

### `POST /api/v1/session-search`

Purpose:

- semantic/hybrid search over raw session history

Behavior:

- search chunk embeddings and optional FTS
- return chunk excerpts and session metadata

## MCP to HTTP mapping

The local `memory-mcp` adapter should map each MCP tool directly to one
HTTP endpoint.

| MCP tool                | HTTP endpoint                            |
|-------------------------|------------------------------------------|
| `memory_store`          | `POST /api/v1/memories`                  |
| `memory_search`         | `POST /api/v1/memories/search`           |
| `memory_get`            | `GET /api/v1/memories/{id}`              |
| `memory_update`         | `PATCH /api/v1/memories/{id}`            |
| `memory_delete`         | `DELETE /api/v1/memories/{id}`           |
| `memory_neighbors`      | `GET /api/v1/memories/{id}/neighbors`    |
| `memory_recall`         | `GET /api/v1/projects/{project}/recall`  |
| `memory_rules`          | `GET /api/v1/projects/{project}/rules`   |
| `memory_bootstrap`      | `GET /api/v1/projects/{project}/bootstrap` |
| `session_start`         | `POST /api/v1/sessions/start`            |
| `session_message_append`| `POST /api/v1/sessions/{id}/messages`    |
| `session_finalize`      | `POST /api/v1/sessions/{id}/finalize`    |
| `review_queue`          | `GET /api/v1/projects/{project}/review-queue` |
| `review_submit`         | `POST /api/v1/review`                    |
| transcript/session hook | `POST /api/v1/sessions/...`              |

The MCP adapter should remain dumb:

- no embeddings
- no local SQLite/Postgres
- no custom ranking logic

## Rollout plan

### Phase 1

- keep the current monolith working
- add HTTP server mode internally
- add OpenAPI generation and Scalar docs
- add bearer auth

### Phase 2

- introduce normalized session tables
- change hooks to write sessions/messages/finalize over HTTP
- keep existing memory endpoints compatible

### Phase 3

- split the current binary into:
  - `memoryd`
  - `memory-mcp`
- point Codex MCP config at `memory-mcp`

### Phase 4

- add web/admin UI against the same HTTP API
- add export/import and pruning workflows

## Open questions

- Whether `session_messages.kind` should be enum-backed in SQL or plain text in v1
- Whether `session_chunks` should reference only session-level chunks or also message-level chunks
- Whether reranking should happen synchronously in v1 or be made configurable per endpoint
- Whether project rules should support precedence/ordering metadata in v1

## Rules Enforcement Flow

Recommended split:

- Hooks enforce deterministic, inspectable constraints that do not
  preempt the client's own permission flow: bootstrap-required checks
  and end-of-session compliance reporting.
- Rule memories carry durable policy and workflow guidance:
  coding standards, repo conventions, review expectations, and
  agent-behavior rules that need to persist across sessions.

Recommended hook trigger points:

- Session start or first prompt: call project bootstrap and
  `POST /api/v1/sessions/start`, then inject the returned rules into
  context.
- Prompt/response/tool hooks: append each event through
  `POST /api/v1/sessions/{id}/messages`.
- Pre-tool or pre-command: re-check rules only when bootstrap state is
  missing or stale.
- Pre-compact/finalize: call `POST /api/v1/sessions/{id}/finalize`, then
  verify whether required rules were loaded and whether any blocked-action
  attempts occurred.

Compliance verification should be explicit:

- log that bootstrap ran for `{project, session_id}`
- log which rule IDs were injected
- fail closed when a required hook cannot confirm bootstrap for risky
  operations
- keep hook logs searchable so later sessions can audit whether the agent
  ignored or never received a rule

## Cross-Agent Review

Recommended durable flow:

- agent A stores a memory tagged `review-needed` (plan, code review, etc.)
- agent B polls `review_queue`, optionally filtering by category
- agent B writes its review with `review_submit`
- both agents can search raw session chronology separately from durable
  review memories

### Normalized session activity timestamps

Appending a message advances `sessions.updated_at` to at least the current
database transaction time, including imports of historical or out-of-order
messages. It also retains any later stored or incoming message timestamp.
Provenance reconciliation can advance this timestamp as well. It represents
session activity, not only the maximum original message timestamp.
