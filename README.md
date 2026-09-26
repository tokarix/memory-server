# memory-server

Semantic memory service and MCP adapter backed by PostgreSQL, pgvector,
and local Ollama models.

Current layout:

```text
agent client <--stdio--> memory-mcp <--HTTP--> memoryd
                                          |
                                          +--> memory-common
                                          |
                                          +--> PostgreSQL + pgvector
                                          |
                                          +--> Ollama
```

Workspace crates:

- `memory-common`: shared config, models, transcript parsing, error
  types, and HTTP/MCP payload types
- `memoryd`: the HTTP service plus `dream` and `ingest` maintenance
  binaries
- `memory-mcp`: the stdio MCP adapter that talks to `memoryd` over HTTP

## Current features

- Persistent semantic memories with embeddings stored in PostgreSQL
- Hybrid retrieval: vector similarity plus PostgreSQL full-text search
- Optional query expansion and LLM reranking for `memory_search`
- Core-memory recall at session start with `memory_recall`
- CRUD tools: store, search, list, get, update, delete
- Memory graph: weighted edges between memories for cross-reference
  traversal and search expansion
- Session transcript archival via `session_log_store`
- Session-log fallback search when no durable memories match
- Maintenance binaries for transcript ingest and dream/prune passes

## Memory categories

| Category | Purpose |
|----------|---------|
| `context` | Project conventions and stable background |
| `decision` | Architectural or workflow decisions |
| `error_fix` | Symptoms, root cause, and resolution |
| `plan` | Reusable implementation plans |
| `rule` | Durable instructions or constraints |

`memory_recall` returns the categories considered core by the current
implementation: `decision`, `error_fix`, `plan`, and `rule`.

Semantic search and standalone recall exclude task-scoped workflow artifacts
by default. These retained audit records include plans tagged with a valid
`task:<uuid>` or legacy `superseded-for-task:<uuid>` marker, earlier Plan
revisions reached recursively through `supersedes-plan:<uuid>`, Decisions that
target a marked Plan through `reviewed-item:<uuid>`, and session transcripts
containing a valid `task:<uuid>` token. Pass
`include_workflow_artifacts=true` to `memory_search` or `memory_recall` to use
the previous inclusive behavior.

The default applies before vector, full-text, graph-neighbor, and session-log
candidate limits, so protected records cannot consume an ordinary result
window. Exact listing and direct retrieval remain inclusive, as do public
neighbor, review, normalized-session, and session-log browsing surfaces.
`memory_bootstrap` retains its stronger policy: its recall payload omits every
Plan and review Decision even though standalone recall has an opt-in.

See the [workflow artifact verification notes](docs/workflow-artifact-verification.md)
for relationship indexing, review contracts, query-plan comparisons, and tests.

## Prerequisites

- Rust 1.88+
- PostgreSQL 17 with `pgvector`
- Ollama with:
  - embedding model: `bge-m3`
  - generation models: defaults use `llama3.1`

## Setup

### 1. Start PostgreSQL with pgvector

Example with Podman:

```sh
podman run -d --name memory-pg \
  -e POSTGRES_DB=memory \
  -e POSTGRES_USER=memory \
  -e POSTGRES_PASSWORD=memory \
  -p 5432:5432 \
  -v memory-pg-data:/var/lib/postgresql/data \
  pgvector/pgvector:pg17
```

### 2. Pull the Ollama models

```sh
ollama pull bge-m3
ollama pull llama3.1
```

### 3. Configure the server

Copy and edit `config.toml.example` if needed:

```toml
http_bind = "127.0.0.1:8080"
memoryd_url = "http://127.0.0.1:8080"
database_url = "postgres://memory:memory@localhost/memory"
ollama_url = "http://localhost:11434"
embedding_model = "bge-m3"
expand_model = "llama3.1"
rerank_model = "llama3.1"
dream_model = "llama3.1"
expand_num_ctx = 8192
rerank_num_ctx = 8192
dream_num_ctx = 8192
```

Configuration fields:

| Field | Default | Purpose |
|-------|---------|---------|
| `database_url` | `postgres://memory:memory@localhost/memory` | PostgreSQL connection string |
| `http_bind` | `127.0.0.1:8080` | Bind address for `memoryd` |
| `memoryd_url` | `http://127.0.0.1:8080` | Base URL used by `memory-mcp` |
| `guardrails_project` | required for MCP | Explicit work project for mandatory guardrails; use `general` for a general-only session |
| `ollama_url` | `http://localhost:11434` | Ollama base URL |
| `embedding_model` | `bge-m3` | Embedding model |
| `embedding_tokenizer_repo` | `None` | HF hub repo (e.g. `BAAI/bge-m3`) to download tokenizer from for guided truncation |
| `embedding_tokenizer_revision` | `main` | HF hub repo revision (branch/tag/SHA) |
| `expand_model` | `llama3.1` | Query expansion model |
| `rerank_model` | `llama3.1` | Search reranking model |
| `dream_model` | `llama3.1` | Dream/prune maintenance model |
| `expand_num_ctx` | `8192` | Context window for query expansion |
| `rerank_num_ctx` | `8192` | Context window for search reranking |
| `dream_num_ctx` | `8192` | Context window for dream maintenance |
| `generate_num_ctx` | `8192` | (Legacy) Context window for all generation calls; used as fallback |

### 4. Build

```sh
cargo build -p memory-mcp --release
cargo build -p memoryd --release
```

When building a specific workspace package from the repository root,
always pass `-p <package>`. For example, use `cargo build -p memory-mcp
--release` instead of `cargo build --bin memory-mcp --release`, because
the latter can still pull in other workspace members and unify their
features.

### 5. Run

```sh
RUST_LOG=info ./target/release/memory-mcp ./config.toml
RUST_LOG=info ./target/release/memoryd ./config.toml
```

`memoryd` runs the HTTP service on `http_bind`. `memory-mcp` is the
stdio adapter that calls `memoryd_url`.

## MCP client setup

Minimal `memory-mcp` config:

```toml
memoryd_url = "http://127.0.0.1:8080"
guardrails_project = "memory-server"
resolution_context = { profile = "workstation-host", phase = "implementing", language = ["rust"] }
# api_token = "replace-me"
```

`memoryd` uses the full server config shown earlier. `memory-mcp` also
requires an explicit `guardrails_project` and a trusted context sufficient to
resolve the policies in that project. Startup fails if the daemon cannot
deliver a nonempty mandatory pack. Configure `api_token` if enabled on the
daemon. See [guardrail delivery](docs/guardrail-delivery.md) for rollout and
reconnection behavior.

### Codex

Add an MCP server entry to `~/.codex/config.toml`:

```toml
[mcp_servers.memory]
command = "/absolute/path/to/target/release/memory-mcp"
args = ["/absolute/path/to/config.toml"]

[mcp_servers.memory.env]
RUST_LOG = "info"
```

### Claude Code

Add a stdio MCP server entry to `~/.claude.json`:

```json
{
  "mcpServers": {
    "memory": {
      "type": "stdio",
      "command": "/absolute/path/to/target/release/memory-mcp",
      "args": ["/absolute/path/to/config.toml"]
    }
  }
}
```

## Multi-agent / distributed setup

For use cases where multiple agents across different machines or locations need
to share a common memory store:

### 1. Server-side configuration

By default, `memoryd` binds to `127.0.0.1`, which only allows local connections.
To make it reachable over a network, set `http_bind` to `0.0.0.0` (all
interfaces) or a specific IP address in your `config.toml`:

```toml
http_bind = "0.0.0.0:8080"
```

### 2. Authentication

When the service is reachable over a network, you **must** enable `api_token` in
the server `config.toml`. The default (no token) is only appropriate for
private localhost use.

```toml
api_token = "your-secure-shared-secret"
```

### 3. Client-side configuration

Each agent's `memory-mcp` needs to know where the central `memoryd` is running.
In each agent's local `config.toml`, set `memoryd_url` to the server's actual
address and include the `api_token`:

```toml
memoryd_url = "http://memory.example.com:8080"
api_token = "your-secure-shared-secret"
```

### 4. Firewall and Reverse Proxy

`memoryd` speaks plain HTTP. For internet-facing setups or when traversing
untrusted networks, it is recommended to run `memoryd` behind a reverse proxy
(such as Nginx or Caddy) that provides TLS (HTTPS) termination.

### 5. Sharing memory across agents

Multiple agents (e.g. Claude Code on a laptop and Codex on a remote server) can
all point to the same `memoryd` instance. They will share all memories
within the same project namespace, enabling cross-agent collaboration and
persistent context.

## HTTP API

`memoryd` currently exposes:

- `GET /api/v1/health`
- `POST /api/v1/memories`
- `POST /api/v1/memories/search`
- `GET /api/v1/memories/{id}`
- `PATCH /api/v1/memories/{id}`
- `DELETE /api/v1/memories/{id}`
- `GET /api/v1/projects/{project}/recall`
- `POST /api/v1/sessions`

If `api_token` is set in config, all `/api/v1/*` routes except health
require `Authorization: Bearer <token>`.

## Available MCP tools

| Tool | Purpose |
|------|---------|
| `memory_server_version` | Return version plus git hash |
| `memory_store` | Store a new memory |
| `memory_search` | Hybrid semantic search within a project |
| `memory_recall` | Load core memories for a project |
| `memory_rules` | Load general + project durable rules |
| `memory_bootstrap` | Load effective rules plus non-rule core recall |
| `memory_list` | Browse memories by project/category |
| `memory_get` | Fetch a single memory by UUID |
| `memory_update` | Update summary/content/tags and re-embed if needed |
| `memory_neighbors` | List neighbor memories reachable via graph edges |
| `memory_delete` | Delete a memory by UUID |
| `session_start` | Create or upsert a normalized shared session |
| `session_message_append` | Append a prompt/response/tool event to a shared session |
| `session_finalize` | Finalize a shared session into searchable chunks |
| `session_log_store` | Store a full session transcript for archival/search |
| `review_queue` | List memories tagged `review-needed`, with optional category filter |
| `review_submit` | Store a review decision and mark the original reviewed |

### Policy identity for Rules

Rules may carry structured `policy` metadata. A legacy Rule has `policy: null`
and remains contextual; existing rows are never classified from their text or
tags. A classified Rule has a project-local `policy_key`, a positive `revision`,
a `delivery_class` of `contextual` or `mandatory`, a server-controlled `state`,
and an optional predecessor UUID in `supersedes`. The active revision is
resolved by `memory_rules` and `memory_bootstrap`; superseded revisions remain
available through `memory_get`, `memory_list`, and search. Applicable mandatory
Rules are delivered regardless of tags or `include_general`.

Classified Rules may also declare `selectors` for `profile`, `phase`,
`language`, and `tool`, plus exact `values` for shared settings. Omitted
selector dimensions match every execution. Selector members within one
dimension are alternatives; dimensions combine. For example,
`{"selectors":{"profile":["workstation-host"],"language":["rust"]},"values":{"rust.build.target_storage":"persistent-disk"}}`
applies only to Rust work on the workstation host. These fields are immutable
within a revision. One active revision remains per `(project, policy_key)`;
environment variants use distinct keys.

MCP execution context comes from the operator's `resolution_context` in
`config.toml`, not from tool arguments, tags, the daemon's host, or model text.
The default is unknown. A tool `context` argument can only assert values
already bound at startup. Direct authenticated HTTP callers may pass a typed
URL-encoded JSON `context` query value. The hook uses the operator-supplied
`MEMORY_RESOLUTION_CONTEXT` JSON environment value. Missing context for a
participating scoped policy fails visibly with `policy_context_required`.
See [policy resolution](docs/policy-resolution.md) for precedence, diagnostics,
canonical output, and rollout details.
The pinned host/container conversion manifest and operator steps are in
[storage policy conversion](docs/storage-policy-conversion.md).

To publish a new root, call `memory_store` with category `rule` and a complete
write payload such as
`"policy":{"policy_key":"build.storage","revision":1,"delivery_class":"contextual"}`.
To revise it, store a new Rule with a higher revision and `supersedes` set to
the current active revision's UUID. Classified content, summary, tags, and
delivery class are immutable; editing them requires a new revision. A key is
1–128 ASCII bytes, starting with a lowercase letter or digit, followed only
by lowercase letters, digits, `.`, `_`, `:`, or `-`.

To classify an existing legacy Rule while preserving its UUID and contents:

1. Call `memory_get(id)` and inspect its project, category, content, summary,
   tags, and `policy: null` status. Copy the `updated_at:` field from the
   metadata header before the blank line that introduces content. It is the
   persisted UTC instant with nine fractional digits and `Z`, for example
   `2026-09-26T12:34:27.123456000Z`. The separate `Updated:` minute display
   is never a concurrency token.
2. Call `memory_update` with only `id`, that exact `expected_updated_at` value,
   and the complete write payload. Example:
   `{"id":"<legacy-rule-uuid>","expected_updated_at":"2026-09-26T12:34:27.123456000Z","policy":{"policy_key":"build.storage","revision":1,"delivery_class":"contextual"}}`.
   Replace the example timestamp with the actual value just read.
3. On success, call `memory_get` on the same UUID to verify the committed
   identity. If the server returns `policy_stale_assignment`, read and inspect
   the changed Rule again before deciding whether to retry. Never refresh the
   token and classify text you have not inspected.

Stop older write-capable `memoryd` instances before enabling classification;
they do not have the new immutability guards. The policy migration rolls back
only while every Rule remains unclassified. Once classification occurs, its
down migration refuses to discard identity and history. Use a separately
authorized export or restore procedure for that case.

`memory_search` behavior:
- expands the user query with the configured LLM
- runs hybrid vector + FTS retrieval against durable memories
- expands seed results via graph edges (same-project by default)
- optionally reranks the combined set with the configured rerank model (disabled by default)
- falls back to session-log search if no durable memories match
- excludes task-scoped workflow artifacts at every retrieval stage unless
  `include_workflow_artifacts=true`

## Additional binaries

### `ingest`

Parses a JSONL transcript file and stores it into `session_logs` and
`session_log_chunks`.

```sh
cargo run -p memoryd --release --bin ingest -- ./config.toml /path/to/transcript.jsonl
```

Dry run:

```sh
cargo run -p memoryd --release --bin ingest -- --dry-run ./config.toml /path/to/transcript.jsonl
```

### `dream`

Runs maintenance passes that merge near-duplicate memories and prune
stale low-importance memories. `plan` and `rule` memories are protected
from these mutations.

```sh
cargo run -p memoryd --release --bin dream -- ./config.toml
```

Dry run:

```sh
cargo run -p memoryd --release --bin dream -- --dry-run ./config.toml
```

## Database notes

Migrations currently create and evolve:

- `memories`
- `session_logs`
- `session_log_chunks`
- full-text search support on memories and session logs
- HNSW vector indexes for semantic retrieval

The current schema is migration-driven. For the next planned shape, see
[`docs/http-api-v1.md`](docs/http-api-v1.md).

## Hooks

A Claude Code `PreCompact` hook script is included at
[`hooks/pre-compact.sh`](hooks/pre-compact.sh). It runs the `ingest`
binary against the session transcript before compaction.

Additional hook scripts are available for durable rule bootstrap and
per-message session capture:

- [`hooks/bootstrap.sh`](hooks/bootstrap.sh): fetches and caches
  `memory_bootstrap` output for the current session and creates the
  normalized remote session row
- [`hooks/capture-message.sh`](hooks/capture-message.sh): append a user or
  assistant message to the normalized remote session stream
- [`hooks/pre-command.sh`](hooks/pre-command.sh): ensures bootstrap state
  exists for the session and records command attempts as session events
- [`hooks/session-stop.sh`](hooks/session-stop.sh): final flush of the
  normalized session into searchable chunks

This lets you save each prompt and each response as the session unfolds,
with agent identity, instead of only storing a transcript at compaction
time.

Example wiring with explicit agent identities:

```json
{
  "hooks": {
    "SessionStart": [{
      "hooks": [{
        "type": "command",
        "command": "/absolute/path/to/memory-server/hooks/bootstrap.sh claude"
      }]
    }],
    "UserPromptSubmit": [{
      "hooks": [{
        "type": "command",
        "command": "/absolute/path/to/memory-server/hooks/capture-message.sh claude user"
      }]
    }],
    "AssistantResponse": [{
      "hooks": [{
        "type": "command",
        "command": "/absolute/path/to/memory-server/hooks/capture-message.sh claude assistant"
      }]
    }],
    "PreToolUse": [{
      "hooks": [{
        "type": "command",
        "command": "/absolute/path/to/memory-server/hooks/pre-command.sh claude"
      }]
    }],
    "PreCompact": [{
      "hooks": [{
        "type": "command",
        "command": "/absolute/path/to/memory-server/hooks/pre-compact.sh"
      }]
    }],
    "Stop": [{
      "hooks": [{
        "type": "command",
        "command": "/absolute/path/to/memory-server/hooks/session-stop.sh"
      }]
    }]
  }
}
```

Notes:

- Use `claude` and `codex` as explicit first arguments if both clients
  write into the same memory service. That makes the stored session events
  attributable during search/finalization.
- The exact event names vary by client. Map these scripts to the closest
  available events in each client.
- The scripts expect `jq` and `curl`.
- They read `memoryd_url` and optional `api_token` from `config.toml`, or
  from `MEMORY_SERVER_CONFIG` if you want to point at another config file.
- Hook state is cached under `/tmp/memory-server-hooks/<external-session-id>/`.

For durable instruction enforcement, prefer the following flow over
duplicating guidance in `AGENTS.md` or `CLAUDE.md`:

- Store durable instructions as `rule` memories. Put cross-project rules
  under project `general`; put repo-specific rules under that repo's
  project name.
- Call `memory_bootstrap(project)` at session start or first prompt in a
  hook so the agent receives the effective rule set plus supporting
  non-rule recall memories.
- Call `memory_rules(project)` from pre-action hooks when only the
  enforceable rule set is needed.
- Keep hooks focused on deterministic enforcement and verification that
  does not preempt the client's own permission flow: ensuring bootstrap
  has happened where needed and recording compliance failures.
- Keep memory rules focused on durable intent and policy that the model
  must follow but that a shell hook cannot reliably derive on its own.

## Memory Graph

Memories are connected by weighted edges stored in the `memory_edges`
table. Edges enable graph-aware search expansion and cross-reference
navigation.

### Edge types

| Relation | Direction | Description |
|----------|-----------|-------------|
| `references` | directed | Explicit reference from one memory to another |
| `related_tag` | undirected | Shared non-structural tags between memories |
| `similar` | undirected | Embedding cosine similarity neighborhood |

### Edge origins

| Origin | When created |
|--------|-------------|
| `content_uuid_ref` | Write-time: UUID found in memory content |
| `structural_tag_ref` | Write-time: structural tag like `plan:<uuid>` |
| `shared_tag` | Dream maintenance: shared topical tags |
| `embedding_neighbor` | Dream maintenance: cosine similarity 0.75–0.92 |
| `usage_reinforcement` | Future: successful retrieval signals |
| `manual` | Future: explicit user/admin edits |

### Search expansion

`memory_search` expands results via graph edges between outer RRF and
LLM reranking. Expansion follows non-suppressed edges with weight ≥ 0.5.

Scope policy (all conservative by default):

- Same-project edges: always followed
- `general` project: only when `include_general=true`
- Foreign projects: only when `cross_project=true`, optionally filtered
  by `project_allowlist`
- Task-scoped workflow artifacts: only when
  `include_workflow_artifacts=true`

Score decay per hop: 0.7×, with additional discounts for `general`
(0.9×) and foreign projects (0.5×).

### Graph maintenance

The `dream` binary includes a graph refresh phase that runs before
merge/prune. It builds `similar` and `related_tag` edges using
idempotent upserts. `ON DELETE CASCADE` on both foreign keys ensures
edges are cleaned up when memories are deleted.

Graph refresh remains inclusive, but merge and prune maintenance exclude
workflow artifacts during candidate generation and recheck provenance inside
the mutation transaction. Historical workflow plans, reviews, and transcripts
are retained for audit and explicit retrieval.

## Review Workflow

For cross-agent collaboration, use the `review-needed` tag on any memory
to request review, and the `review_queue`/`review_submit` tools to
manage the workflow.

### Plan reviews

- Claude stores a `plan` memory tagged `review-needed`.
- Codex calls `review_queue(project, category: "plan")` to find pending
  plan reviews.
- Codex reviews the plan and calls `review_submit(...)`.
- `review_submit` stores a `decision` memory linked to the plan and
  updates the original plan tags from `review-needed` to `reviewed`.

### Code reviews

- An agent stores a `context` memory tagged `review-needed` and
  `code-review`, with git range and description in the content.
- Another agent calls `review_queue(project)` or
  `review_queue(project, category: "context")` to find pending reviews.
- The reviewer inspects the code and calls `review_submit(...)`.
- `review_submit` stores a `decision` memory and retags the original.

## Worker Workflows

When launching short-lived, targeted worker sessions, you must avoid context dilution. Unrestricted bootstraps (`memory_bootstrap`) in highly specialized workers (e.g. ones that solely write frontend CSS vs ones that manage SQL migrations) will pollute the AI's context with rules and guidelines meant for entirely different phases of the project.

For isolated workers, configure the execution context at MCP startup. Use
`include_recall=false` when only policies are needed. Tags can reduce optional
contextual guidance, but they never select an execution profile or suppress
applicable mandatory policies. Rules and memories can still carry tags such as
`lang:rust` or `phase:planning` for contextual retrieval.

1. At session start, specialized workers call `memory_rules(project)` with a trusted startup context; optional tags filter contextual guidance only.
2. For retrieval, workers must exclusively use `memory_search(tags=...)` targeted to their operational domain.
3. If creating rules or plans intended for specialized agents, always ensure they are tagged with the relevant `lang:*` or `phase:*` identifiers.

## Skills

Repo-managed skills live under [`skills/`](skills/).

Current shared skill:

- [`skills/review/SKILL.md`](skills/review/SKILL.md): unified workflow
  for requesting and performing reviews (plans, code, etc.) with
  `review_queue` and `review_submit`

Install symlink(s) for local clients with:

```sh
./scripts/install-skills.sh all
```

Or target one client:

```sh
./scripts/install-skills.sh codex
./scripts/install-skills.sh claude
```

This symlinks the repo-managed skill into:

- `~/.codex/skills/review`
- `~/.claude/skills/review`

## Development

Run tests with:

```sh
cargo test
```

Useful files:
- [`src/tools.rs`](src/tools.rs): MCP tool surface
- [`src/db.rs`](src/db.rs): SQL access layer
- [`src/transcript.rs`](src/transcript.rs): JSONL transcript parsing and chunking
- [`docs/http-api-v1.md`](docs/http-api-v1.md): planned HTTP API split
