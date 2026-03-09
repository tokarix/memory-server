# memory-server

Semantic memory MCP server backed by PostgreSQL, pgvector, and local
Ollama models.

Current layout:

```text
agent client <--stdio--> memory-server / memory-mcp
                                     |
                                     +--> shared app core <---> PostgreSQL + pgvector
                                                          |
                                                          +--> Ollama

web/admin/other clients <--HTTP--> memoryd
```

Planned HTTP split:
- [`docs/http-api-v1.md`](docs/http-api-v1.md): proposed `memoryd` +
  `memory-mcp` architecture with `/api/v1`, generated OpenAPI, and
  Scalar docs

## Current features

- Persistent semantic memories with embeddings stored in PostgreSQL
- Hybrid retrieval: vector similarity plus PostgreSQL full-text search
- Query expansion and LLM reranking for `memory_search`
- Core-memory recall at session start with `memory_recall`
- CRUD tools: store, search, list, get, update, delete
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
ollama_model = "bge-m3"
expand_model = "llama3.1"
rerank_model = "llama3.1"
dream_model = "llama3.1"
generate_num_ctx = 8192
```

Configuration fields:

| Field | Default | Purpose |
|-------|---------|---------|
| `database_url` | `postgres://memory:memory@localhost/memory` | PostgreSQL connection string |
| `http_bind` | `127.0.0.1:8080` | Bind address for `memoryd` |
| `memoryd_url` | `http://127.0.0.1:8080` | Base URL used by `memory-mcp` |
| `ollama_url` | `http://localhost:11434` | Ollama base URL |
| `ollama_model` | `bge-m3` | Embedding model |
| `expand_model` | `llama3.1` | Query expansion model |
| `rerank_model` | `llama3.1` | Search reranking model |
| `dream_model` | `llama3.1` | Dream/prune maintenance model |
| `generate_num_ctx` | `8192` | Context window for generation calls |

### 4. Build

```sh
cargo build --release
```

### 5. Run

```sh
RUST_LOG=info ./target/release/memory-server ./config.toml
```

Alternative binaries:

```sh
RUST_LOG=info ./target/release/memory-mcp ./config.toml
RUST_LOG=info ./target/release/memoryd ./config.toml
```

`memory-server` remains the monolith entrypoint and runs migrations
locally. `memoryd` runs the HTTP service on `http_bind`, and
`memory-mcp` is the stdio adapter that calls `memoryd_url`.

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
| `memory_list` | Browse memories by project/category |
| `memory_get` | Fetch a single memory by UUID |
| `memory_update` | Update summary/content/tags and re-embed if needed |
| `memory_delete` | Delete a memory by UUID |
| `session_log_store` | Store a full session transcript for archival/search |

`memory_search` behavior:
- expands the user query with the configured LLM
- runs hybrid vector + FTS retrieval against durable memories
- reranks results with the configured rerank model
- falls back to session-log search if no durable memories match

## Additional binaries

### `ingest`

Parses a JSONL transcript file and stores it into `session_logs` and
`session_log_chunks`.

```sh
cargo run --release --bin ingest -- ./config.toml /path/to/transcript.jsonl
```

Dry run:

```sh
cargo run --release --bin ingest -- --dry-run ./config.toml /path/to/transcript.jsonl
```

### `dream`

Runs maintenance passes that merge near-duplicate memories and prune
stale low-importance memories. `plan` and `rule` memories are protected
from these mutations.

```sh
cargo run --release --bin dream -- ./config.toml
```

Dry run:

```sh
cargo run --release --bin dream -- --dry-run ./config.toml
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
