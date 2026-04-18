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

## Prerequisites

- Rust 1.85+
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
# api_token = "replace-me"
```

`memoryd` uses the full server config shown earlier. `memory-mcp` only
needs `memoryd_url` and, if enabled on the server, `api_token`.

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

`memory_search` behavior:
- expands the user query with the configured LLM
- runs hybrid vector + FTS retrieval against durable memories
- expands seed results via graph edges (same-project by default)
- optionally reranks the combined set with the configured rerank model (disabled by default)
- falls back to session-log search if no durable memories match

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

Score decay per hop: 0.7×, with additional discounts for `general`
(0.9×) and foreign projects (0.5×).

### Graph maintenance

The `dream` binary includes a graph refresh phase that runs before
merge/prune. It builds `similar` and `related_tag` edges using
idempotent upserts. `ON DELETE CASCADE` on both foreign keys ensures
edges are cleaned up when memories are deleted.

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

For isolated workers, you should completely avoid `memory_bootstrap` or pass `include_recall=false` minus the general rules, and instead rely on **Tag-Based Filtering**. Rules and memories can be scoped using tags such as `lang:rust`, `lang:typescript`, `phase:planning`, or `phase:styling`. 

1. At session start, specialized workers MUST be instructed to call `memory_rules(project, tags=["lang:rust"])` or multi-tag combinations like `tags=["lang:rust", "phase:planning"]`.
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
