# memory-server

Semantic memory MCP server for Claude Code. Provides persistent,
searchable memory across sessions using vector similarity search.

Claude Code loses context between sessions and within long
conversations. Rules get forgotten, past decisions aren't referenced,
errors get re-debugged. This server gives Claude Code a structured,
searchable long-term memory backed by PostgreSQL, pgvector, and Ollama
embeddings.

## Architecture

```
Claude Code <--stdio--> memory-server <---> PostgreSQL + pgvector
                              |
                              +-----------> Ollama (BGE-M3 embeddings)
```

The server communicates with Claude Code over MCP stdio transport.
When storing or searching memories, it calls the local Ollama instance
to generate 1024-dimensional embeddings using BGE-M3, then uses
pgvector's HNSW index for cosine similarity search.

### Memory categories

| Category     | Purpose                                            |
|--------------|----------------------------------------------------|
| `context`    | Project conventions, current state, patterns        |
| `decision`   | Architectural/design decisions with reasoning       |
| `error_fix`  | Symptoms, root causes, and fixes for past errors    |

### Embedding strategy

Embedding input is `"{summary}\n\n{content}"` — the model sees both a
concise label and the full detail, which improves retrieval quality
compared to embedding either field alone.

## Prerequisites

- **Rust** 1.85+ (edition 2024)
- **PostgreSQL 17** with pgvector extension
- **Ollama** with `bge-m3` model pulled

## Setup

### 1. PostgreSQL with pgvector

pgvector may not be available in your OS repos. The simplest approach
is the official container image, which is stock PostgreSQL with
pgvector pre-installed:

```sh
podman run -d --name memory-pg \
  -e POSTGRES_DB=memory \
  -e POSTGRES_USER=memory \
  -e POSTGRES_PASSWORD=memory \
  -p 5432:5432 \
  -v memory-pg-data:/var/lib/postgresql/data \
  pgvector/pgvector:pg17
```

Or with Docker:

```sh
docker run -d --name memory-pg \
  -e POSTGRES_DB=memory \
  -e POSTGRES_USER=memory \
  -e POSTGRES_PASSWORD=memory \
  -p 5432:5432 \
  -v memory-pg-data:/var/lib/postgresql/data \
  pgvector/pgvector:pg17
```

### 2. Run the migration

```sh
psql postgres://memory:memory@localhost/memory \
  < migrations/20260227000000_init.sql
```

This creates:
- The `vector` extension
- The `memory_category` enum (`context`, `decision`, `error_fix`)
- The `memories` table with a 1024-dimensional vector column
- An HNSW index for cosine similarity search
- B-tree indexes on `category`, `project`, and `(project, category)`

### 3. Ollama

Install Ollama and pull the embedding model:

```sh
ollama pull bge-m3
```

BGE-M3 produces 1024-dimensional embeddings and supports multilingual
input. The model runs locally — no data leaves your machine.

### 4. Build

```sh
cargo build --release
```

The binary is at `target/release/memory-server` (~13 MB).

### 5. Register with Claude Code

Add to `~/.claude.json`:

```json
{
  "mcpServers": {
    "memory": {
      "type": "stdio",
      "command": "/absolute/path/to/memory-server",
      "args": ["/absolute/path/to/config.toml"]
    }
  }
}
```

The config file argument is optional. Without it, all settings use
their defaults.

## Configuration

All fields are optional and have sensible defaults. Copy
`config.toml.example` and adjust as needed:

```toml
database_url = "postgres://memory:memory@localhost/memory"
ollama_model = "bge-m3"
ollama_url = "http://localhost:11434"
```

| Field          | Default                                       | Description                        |
|----------------|-----------------------------------------------|------------------------------------|
| `database_url` | `postgres://memory:memory@localhost/memory`   | PostgreSQL connection string       |
| `ollama_model` | `bge-m3`                                      | Ollama model for embeddings        |
| `ollama_url`   | `http://localhost:11434`                      | Ollama API base URL                |

Zero-config works if the PostgreSQL `memory` database exists with
the migration applied, and Ollama runs on the default port.

### Logging

Logs go to stderr (stdout is the MCP transport). Control verbosity
with `RUST_LOG`:

```sh
RUST_LOG=debug memory-server config.toml
RUST_LOG=memory_server=trace memory-server
```

## MCP Tools

### memory_store

Store a new memory. The content and summary are embedded via Ollama
and stored alongside the text for later semantic retrieval.

**Parameters:**

| Name       | Type       | Required | Description                              |
|------------|------------|----------|------------------------------------------|
| `category` | string     | yes      | `context`, `decision`, or `error_fix`    |
| `content`  | string     | yes      | Full content of the memory               |
| `project`  | string     | yes      | Project this memory belongs to           |
| `summary`  | string     | yes      | Brief summary for display and embedding  |
| `tags`     | string[]   | no       | Tags for organization                    |

**Returns:** Confirmation with the generated UUID, category, and summary.

**Example call from Claude Code:**

```
Store a decision memory for project "myapp":
  category: decision
  summary: Use pgvector for semantic search
  content: Evaluated pgvector, Qdrant, and Pinecone. pgvector wins
           because it runs inside PostgreSQL (no extra service), has
           HNSW indexes, and the pgvector Rust crate integrates
           directly with sqlx.
  tags: database, architecture, search
```

---

### memory_search

Semantic search across stored memories. The query is embedded via
Ollama and compared against all stored embeddings using cosine
similarity. Results are ranked by relevance.

**Parameters:**

| Name       | Type     | Required | Default | Description                           |
|------------|----------|----------|---------|---------------------------------------|
| `query`    | string   | yes      |         | Natural language search query         |
| `project`  | string   | yes      |         | Project to search within              |
| `category` | string   | no       |         | Filter to a specific category         |
| `limit`    | integer  | no       | 5       | Maximum number of results             |

**Returns:** Ranked list with similarity scores, formatted as markdown.

**Example output:**

```markdown
## Search Results (2 matches)

### 1. [decision] Use pgvector for semantic search (similarity: 0.89)
ID: 550e8400-e29b-41d4-a716-446655440000
Tags: database, architecture, search
Created: 2025-06-15 12:00

Evaluated pgvector, Qdrant, and Pinecone. pgvector wins because it
runs inside PostgreSQL (no extra service), has HNSW indexes, and the
pgvector Rust crate integrates directly with sqlx.

---

### 2. [context] Database connection pooling setup (similarity: 0.72)
ID: 6ba7b810-9dad-11d1-80b4-00c04fd430c8
Tags: database, infrastructure
Created: 2025-06-14 09:30

Using sqlx PgPoolOptions with max_connections=5. Connection string
comes from config.toml.

---
```

**How similarity search works:**

1. The query string is sent to Ollama's `/api/embed` endpoint
2. The returned 1024-dim vector is compared against all stored
   embeddings using the pgvector `<=>` cosine distance operator
3. PostgreSQL uses the HNSW index to avoid a full table scan
4. Results are returned as `1 - distance` (similarity, 0.0 to 1.0)
5. `ORDER BY` uses the raw `<=>` expression so the index is used;
   the similarity alias is computed but not used for ordering

---

### memory_list

Browse memories by project with optional category filter. Results are
paginated and ordered by most recently updated first.

**Parameters:**

| Name       | Type     | Required | Default | Description                           |
|------------|----------|----------|---------|---------------------------------------|
| `project`  | string   | yes      |         | Project to list memories for          |
| `category` | string   | no       |         | Filter to a specific category         |
| `limit`    | integer  | no       | 20      | Maximum number of results             |
| `offset`   | integer  | no       | 0       | Skip this many results (pagination)   |

**Returns:** Formatted markdown list of memories with metadata.

---

### memory_update

Partial update of an existing memory. Only the fields you provide are
changed; others are left untouched. If `content` or `summary` changes,
the embedding is automatically regenerated.

**Parameters:**

| Name      | Type     | Required | Description                              |
|-----------|----------|----------|------------------------------------------|
| `id`      | UUID     | yes      | UUID of the memory to update             |
| `content` | string   | no       | New content (triggers re-embedding)      |
| `summary` | string   | no       | New summary (triggers re-embedding)      |
| `tags`    | string[] | no       | New tags (replaces existing tags)        |

**Re-embedding behavior:** If either `content` or `summary` is
provided, the server generates a new embedding from the provided
fields. If only `summary` is given, the content portion of the
embedding input is empty (and vice versa). For best results, provide
both when changing either.

**Returns:** Confirmation or "not found" error.

---

### memory_delete

Delete a memory by its UUID. This is permanent — there is no soft
delete or undo.

**Parameters:**

| Name | Type | Required | Description                  |
|------|------|----------|------------------------------|
| `id` | UUID | yes      | UUID of the memory to delete |

**Returns:** Confirmation or "not found" error.

## Database Schema

```sql
CREATE EXTENSION IF NOT EXISTS vector;

CREATE TYPE memory_category AS ENUM ('context', 'decision', 'error_fix');

CREATE TABLE memories (
    id          UUID PRIMARY KEY,
    category    memory_category NOT NULL,
    content     TEXT NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    embedding   VECTOR(1024) NOT NULL,
    project     TEXT NOT NULL,
    summary     TEXT NOT NULL,
    tags        TEXT[] NOT NULL DEFAULT '{}',
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
```

Fields are ordered alphabetically after `id`.

### Indexes

| Index                            | Type  | Purpose                                   |
|----------------------------------|-------|-------------------------------------------|
| `idx_memories_embedding`         | HNSW  | Cosine similarity search on vectors       |
| `idx_memories_category`          | B-tree| Filter by category                        |
| `idx_memories_project`           | B-tree| Filter by project                         |
| `idx_memories_project_category`  | B-tree| Combined project + category filter        |

The HNSW index uses `vector_cosine_ops` for cosine distance. HNSW
(Hierarchical Navigable Small World) provides approximate nearest
neighbor search with high recall and sub-linear query time.

## Project Structure

```
memory-server/
  Cargo.toml                         # Dependencies and build config
  config.toml.example                # Example configuration file
  migrations/
    20260227000000_init.sql           # Database schema
  src/
    config.rs                        # TOML config loading with defaults
    db.rs                            # PostgreSQL connection pool, CRUD, vector search
    embed.rs                         # Ollama /api/embed HTTP client
    error.rs                         # Error types with MCP ErrorData conversion
    main.rs                          # Entrypoint: config, pool, embed client, stdio server
    model.rs                         # Category enum and Memory struct
    tools.rs                         # 5 MCP tool definitions with parameter types
```

### Module dependency graph

```
main.rs
  +-- config.rs          (TOML deserialization, defaults)
  +-- db.rs              (sqlx queries, pgvector)
  |     +-- model.rs     (Category, Memory)
  +-- embed.rs           (reqwest -> Ollama)
  |     +-- error.rs     (Error enum)
  +-- error.rs           (thiserror, MCP ErrorData conversion)
  +-- model.rs           (data types)
  +-- tools.rs           (rmcp tool definitions)
        +-- db.rs
        +-- embed.rs
        +-- error.rs
        +-- model.rs
```

### Key dependencies

| Crate                | Version | Purpose                                      |
|----------------------|---------|----------------------------------------------|
| `rmcp`               | 0.16    | MCP protocol SDK, stdio transport, tool macros|
| `sqlx`               | 0.8     | Async PostgreSQL driver                      |
| `pgvector`           | 0.4     | Vector type for sqlx                         |
| `reqwest`            | 0.13    | HTTP client for Ollama API                   |
| `chrono`             | 0.4     | Timestamps with timezone                     |
| `schemars`           | 1.0     | JSON Schema generation for MCP tool params   |
| `serde` / `toml`     | 1.0     | Config deserialization                       |
| `thiserror`          | 2.0     | Error type derivation                        |
| `tokio`              | 1.49    | Async runtime                                |
| `tracing`            | 0.1     | Structured logging (to stderr)               |
| `uuid`               | 1.21    | UUIDv4 generation for memory IDs             |

## Development

### Checks

```sh
cargo fmt --check
cargo clippy --all-features --all-targets --no-deps -- -Dclippy::pedantic
cargo test
```

All three must pass before every commit.

### Running locally

```sh
# Start PostgreSQL
podman start memory-pg

# Start Ollama (if not already running)
ollama serve &

# Run the server (it communicates over stdin/stdout)
cargo run -- config.toml
```

Since the server uses stdio, you can't interact with it directly from
a terminal in a useful way. It's designed to be launched by Claude Code
as an MCP server.

### Test suite

16 unit tests covering:

- **config** (3): default values, partial deserialization, empty config
- **embed** (3): request serialization, response deserialization,
  empty response handling
- **error** (3): display formatting, MCP ErrorData conversion, error codes
- **model** (3): category display, serde roundtrip, alphabetical ordering
- **tools** (4): list formatting, search formatting, empty list, empty search

Database-dependent operations (insert, search, etc.) are not covered
by unit tests since they require a running PostgreSQL instance. See
the "Future Improvements" section for integration test plans.

## Error Handling

Errors are categorized with MCP error codes:

| Code     | Category    | Cause                                  |
|----------|-------------|----------------------------------------|
| `-32000` | `Database`  | PostgreSQL connection or query failure  |
| `-32001` | `Embedding` | Ollama unreachable, model error, etc.  |

All errors include the full error message in the MCP response. The
`Database` variant uses `#[from] sqlx::Error` for automatic
conversion. The `Embedding` variant carries a formatted string with
the HTTP status and response body from Ollama when available.

## Future Improvements

### Enforced rules via memory

**Complexity: low**

Add a `rule` category for project-enforced rules that must always be
followed. A dedicated `memory_rules` tool returns all active rules for
a project, designed to be called at session start or via a Claude Code
hook. This turns the memory server into an enforceable instruction
layer — rules stored here override defaults and persist across
sessions.

Implementation: add `Rule` to the `Category` enum, a new
`memory_rules` tool that lists all rules for a project (no
pagination, always returns everything), and a migration to add the
enum value.

### Auto-save disagreements and consensus

**Complexity: low**

Add a `consensus` category for recording instances where the user and
Claude Code disagreed, deliberated, and reached a resolution. Each
memory stores the initial positions, the reasoning, and the final
decision. Over time, this builds a corpus of calibration data —
Claude Code can search past disagreements to avoid repeating the same
arguments and to better predict user preferences.

Implementation: add `Consensus` to the `Category` enum with a
`memory_consensus` tool that accepts structured fields (initial
position, counterargument, resolution). The semantic search naturally
surfaces relevant past disagreements when similar topics arise.

### Duplicate detection

**Complexity: low**

Before storing a new memory, run a similarity search and warn if a
highly similar memory already exists (e.g., similarity > 0.92). This
prevents the same decision or fix from being recorded multiple times
with slightly different wording. The tool could return the existing
memory and ask whether to update it instead.

Implementation: add a similarity check to `memory_store` before
inserting. If a near-duplicate is found, return it in the response
with a suggestion to use `memory_update` instead.

### Memory decay and relevance scoring

**Complexity: medium**

Add a `relevance_score` or `access_count` column that tracks how
often a memory is retrieved. Memories that are never accessed could
be flagged for review or archival. Combine recency (updated_at) with
access frequency to produce a composite relevance score for ranking.

Implementation: add an `access_count INTEGER DEFAULT 0` and
`last_accessed TIMESTAMPTZ` column. Increment on every search hit.
Add a `memory_prune` tool that lists stale memories (old, never
accessed). Optionally, a decay function that reduces relevance
scores over time.

### Full-text search fallback

**Complexity: medium**

Add PostgreSQL full-text search (`tsvector`/`tsquery`) as a fallback
when semantic search returns low-similarity results. This catches
exact keyword matches that embedding models might miss (e.g., error
codes, function names, specific identifiers). The tool could run
both searches in parallel and merge results.

Implementation: add a `search_vector TSVECTOR` column with a GIN
index, populate it via a trigger on insert/update, and add a
`memory_search_text` tool (or enhance `memory_search` with a
`mode` parameter: `semantic`, `keyword`, `hybrid`).

### Migration management

**Complexity: medium**

Replace the manual `psql < migration.sql` step with sqlx's built-in
migration runner. The server would check and apply pending migrations
on startup, making deployment zero-touch.

Implementation: use `sqlx::migrate!()` macro with the `migrations/`
directory. This requires adding the `migrate` feature to sqlx and
running `sqlx migrate` commands during development. The macro embeds
migrations in the binary at compile time.

### IRC bridge

**Complexity: medium**

A separate service that bridges an IRC channel to Claude Code
sessions. The bot joins a configured channel, forwards messages to
Claude Code (via the Anthropic API or a local model), and relays
responses back. This allows interaction through IRC — useful for
monitoring, quick queries, or collaborative debugging from a
preferred communication medium.

Implementation: separate binary using the `irc` crate for the IRC
client, `reqwest` for the Anthropic API, and a message queue (tokio
channels) to serialize concurrent conversations. The bot would need
conversation state management, rate limiting, and authentication
(only respond to configured nicks/channels).

### Local LLM review layer

**Complexity: medium-high**

A second MCP server (or additional tool in this one) that sends
proposed changes to a local Ollama model for independent review.
Every code change gets a second opinion from a model with a clean
context window — no accumulated biases from the current conversation.
The reviewer model could check for rule violations, logical errors,
or missed edge cases.

Implementation: a `review_changes` tool that accepts a diff and
context, sends it to a local model (e.g., Qwen 2.5 32B, Llama 3.3
70B, or DeepSeek R1 via Ollama), and returns structured feedback.
Challenges include context window limits of local models, response
quality compared to Claude, and latency (large local models are
slow).

### Cross-project memory sharing

**Complexity: medium**

Allow memories to be shared across projects or linked between them.
A decision made in one project (e.g., "always use rustls, never
openssl") might apply to all Rust projects. Add a `global` project
scope or a linking mechanism.

Implementation: add a `scope` field (`project`, `global`) or allow
`project` to be a comma-separated list. Modify search to optionally
include global memories. Alternatively, add a `memory_link` tool
that creates cross-references between memories in different projects.

### Embedding model hot-swap

**Complexity: medium**

Allow changing the embedding model without re-embedding everything
immediately. Store the model name alongside each embedding. When the
configured model changes, new memories use the new model while old
ones keep working. Add a `memory_reindex` tool that re-embeds all
memories with the current model in the background.

Implementation: add a `model TEXT NOT NULL DEFAULT 'bge-m3'` column.
The search query would need to handle mixed-model scenarios (either
by filtering to the current model or by always re-embedding queries
with all models used in the corpus). The `memory_reindex` tool would
iterate through all memories and regenerate embeddings.

### Backup and export

**Complexity: low**

Add a `memory_export` tool that dumps all memories for a project as
JSON (without embeddings — they can be regenerated). This enables
backup, migration between instances, and sharing memory corpora.
Pair with a `memory_import` tool that loads a JSON dump and
re-embeds everything.

Implementation: a `memory_export` tool that serializes memories
to JSON and a `memory_import` tool that deserializes, generates
embeddings, and inserts. The export format should be stable and
versioned.

### Conversation summarization

**Complexity: high**

Automatically summarize completed Claude Code sessions and store key
decisions, errors encountered, and patterns observed as memories.
This removes the need for manual `memory_store` calls — the server
would observe the conversation and extract noteworthy items.

Implementation: requires access to the conversation transcript
(possibly via Claude Code hooks or a post-session trigger). A
summarization model (either Claude via API or a local model) would
extract structured memories from the transcript. This is the most
complex improvement because it requires conversation access,
summarization quality, and deduplication against existing memories.

### Web UI for memory browsing

**Complexity: high**

A web interface for browsing, searching, editing, and managing
memories outside of Claude Code. Useful for reviewing what Claude
Code has learned, bulk editing, and monitoring memory growth.

Implementation: add an HTTP server (axum or actix-web) alongside
the stdio MCP server, or as a separate binary sharing the same
database. The UI would be a simple SPA (or server-rendered HTML)
with search, filter, and CRUD functionality. Could reuse the same
`db.rs` query layer.

### Multi-tenant / team memory

**Complexity: high**

Support multiple users sharing a memory server instance, with
per-user and shared team memories. Requires authentication (API
keys or tokens), access control (who can read/write which
projects), and isolation between tenants.

Implementation: add a `user_id` column, authentication middleware
in the MCP transport layer, and access control checks in every
tool handler. This fundamentally changes the architecture from a
single-user local tool to a shared service.
