# memory-server

Semantic memory MCP server for Claude Code. Provides persistent,
searchable memory across sessions using vector similarity search.

See `docs/http-api-v1.md` for the planned split into `memoryd` plus a
thin `memory-mcp` adapter, with a versioned `/api/v1` HTTP API and
Scalar docs.

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

## Known Issues and Limitations

Issues identified through critical review. These should be addressed
before production use.

### 1. No automatic recall trigger

The biggest limitation: Claude Code will not call `memory_search`
spontaneously unless explicitly instructed. Without a session-start
hook or a `CLAUDE.md` instruction like "at the start of every session
call `memory_search` for the current project", the memory server is
invisible. The tools exist but the trigger mechanism does not.

**Mitigation**: add to your project's `CLAUDE.md`:
```
At session start, call memory_search with project "<name>" and query
"current conventions, active decisions, and recent errors" to load
relevant context.
```

### 2. Embedding asymmetry between store and search

Stored memories embed `"{summary}\n\n{content}"` (full context),
but search queries embed `"{query}\n\n"` (query only, empty content).
This means query vectors live in a different distribution than memory
vectors. BGE-M3 is relatively robust to this, but short queries
against long memories will have reduced recall.

**Impact**: "database connection pooling" as a 3-word query will not
match as well against a 200-word memory about connection pooling that
was embedded with its full content.

### 3. Partial update re-embedding is broken

`memory_update` with only `summary` set creates an embedding from
`"{new_summary}\n\n"` — the full stored content is not read from the
database and not included. The resulting embedding is wrong and will
not represent the actual stored memory.

**Fix needed**: fetch the existing memory from the database before
computing the re-embedding, and use the stored fields for any field
not provided in the update.

### 4. No similarity threshold on search

`memory_search` returns up to `limit` results regardless of
similarity score. If no memories are relevant, it returns the
least-irrelevant ones (e.g., similarity 0.35). Claude Code has no
signal that these are noise. A minimum similarity threshold
(default 0.5) should exclude low-quality results.

### 5. Project name is not inferred

Every tool call requires a `project` name supplied by the caller.
Claude Code does not have a built-in "current project" concept. If
session 1 stores under `"memory-server"` and session 2 searches under
`"memory_server"`, all memories are invisible. There is no
normalization, no fuzzy matching, no fallback.

**Mitigation**: use a `.memory-project` file in the project root, or
always use the git repository name as the project identifier.

### 6. Stale memories are indistinguishable from current ones

After 6 months, the corpus will contain superseded decisions with no
staleness signal. `updated_at` is stored but not used in search
ranking. A decision from January that was replaced in June will still
surface with high similarity. This is actively harmful.

### 7. List queries fetch unused embeddings

`db::list` fetches the full `Memory` struct including the 1024-element
`Vec<f32>` embedding (4 KB per row), then `format_memory_list`
ignores it. The list path should use a projection that excludes the
embedding column.

### 8. No `memory_get` tool

There is no way to retrieve a single memory by UUID. The
fetch-before-update workflow requires `memory_list`, visual scanning,
and UUID extraction. A `memory_get(id)` tool is trivially useful.

### 9. Three-service operational burden

Running PostgreSQL, Ollama, and memory-server with no graceful
degradation. If Ollama is down, every tool call fails. There is no
health check, no retry logic, no lazy connection, no fallback. The
server exits immediately if PostgreSQL is unreachable at startup.

### 10. No input validation on limit/offset

`limit` and `offset` are `i64`. Negative values are accepted and
produce undefined behavior in PostgreSQL. Should be clamped:
`limit.max(1).min(100)`, `offset.max(0)`.

## Design Analysis

Critical analysis of the architecture and its trade-offs, informed by
review and by comparison with the [qorvus](https://github.com/stintel/qorvus)
memory system which implements a similar vector search architecture
with pluggable SQLite/PostgreSQL backends.

### Is semantic search the right approach?

For the stated use cases — error fixes, decisions, conventions —
semantic search is frequently overkill and sometimes counterproductive:

- **Error messages are precise.** Searching for "cannot borrow as
  mutable" does not benefit from fuzzy embedding matching; it benefits
  from exact substring search. Semantic embeddings are noisy for
  highly specific technical strings.
- **Small corpus sizes.** A realistic 6-month corpus is 300-800
  memories. At 4 KB per embedding, that is 3 MB of vector data. HNSW
  is not needed — a linear scan would be equivalent in latency. The
  B-tree indexes on `project` and `category` do the heavy lifting.
- **Hybrid search should be day-one.** Full-text search with
  `tsvector`/`tsquery` catches exact keyword matches that embeddings
  miss (error codes, function names, identifiers). This is listed as
  a future improvement but should be in the initial implementation.

The qorvus project validates the overall approach: its memory system
uses the same architecture (Ollama embeddings + pgvector cosine
similarity + HNSW indexing) and works well in practice. The key
difference is that qorvus has automatic memory recall — it
pre-searches memory before every user message and injects results as
context. This "always-on" approach is what makes memory useful.

### Operational burden: the Ollama problem

Ollama must be running, BGE-M3 must be loaded into RAM (~570 MB),
and the first embed call after cold start takes 1-3 seconds on CPU.
On laptops that suspend/resume, Ollama frequently dies or hangs.

**Alternative**: `fastembed-rs` (wraps ONNX Runtime) supports BGE-M3
natively, downloads the model on first run, and handles inference
in-process. This eliminates the Ollama dependency entirely, reducing
the operational burden from three services to one (PostgreSQL only).

**Alternative**: a SQLite backend with in-process cosine similarity
(as qorvus implements) eliminates the PostgreSQL dependency too,
making the server fully self-contained. Suitable for small corpora
(< 50K entries). The qorvus SQLite backend stores embeddings as
little-endian f32 BLOBs and computes cosine similarity in Rust.

### Corpus quality degradation over time

Without guardrails, the memory corpus will accumulate:

- **Duplicates**: same decision stored multiple times with different
  wording, diluting search results
- **Stale entries**: superseded decisions that contradict current ones,
  with no signal about which is authoritative
- **Noise**: low-quality entries ("I ran cargo fmt today") that
  reduce overall search precision
- **Inconsistent project names**: same project stored under multiple
  spellings, making memories invisible across sessions

The current design has no deduplication, no versioning, no staleness
tracking, and no quality gate. These problems compound over months.

### Prompt injection via stored memories

If an adversary can cause a memory to be stored (e.g., a malicious
repository that causes Claude Code to call `memory_store` with
crafted content), every future session searching that project's
memories will receive the injected instructions. The attacker only
needs to trigger one `memory_store` call with adversarial content.

There is no sanitization of stored content, no review step, and no
flagging of instruction-like content. This is a systemic risk with
any LLM memory system. Mitigations include: requiring user
confirmation before storing, rate-limiting autonomous storage, or
content validation.

### Interaction with Claude Code's existing memory

Claude Code already has `CLAUDE.md` (project instructions) and
auto-memory (`MEMORY.md`). These are simpler, require zero
infrastructure, and are always available. The memory-server adds:

- Semantic search (CLAUDE.md does not have this)
- Machine-writable storage from within sessions
- Structured categories and per-project scoping
- Scalability beyond what fits in a markdown file

The value proposition is specifically autonomous storage and
retrieval. If Claude Code can be trusted to store and retrieve
effectively, the value is real. If the user has to manually trigger
every operation, `CLAUDE.md` is strictly better.

## Future Improvements

Organized by priority. Items marked **[critical]** address known
issues above. Items marked **[qorvus]** are informed by patterns
in the qorvus memory system.

### Priority 1: Correctness fixes

#### Fix partial update re-embedding [critical]

Fetch the existing memory from the database before computing the
re-embedding. Use stored fields for any field not provided in the
update request.

```rust
// Before re-embedding, fetch current state:
let current = db::get(&self.pool, params.id).await?;
let summary = params.summary.as_deref().unwrap_or(&current.summary);
let content = params.content.as_deref().unwrap_or(&current.content);
let embedding = self.embed_client.embed(summary, content).await?;
```

#### Add similarity threshold to search [critical]

Filter results below a configurable minimum similarity (default 0.5).
Return "no relevant memories found" instead of low-quality noise.

```sql
WHERE project = $2
  AND 1 - (embedding <=> $1) >= $4  -- minimum similarity
ORDER BY embedding <=> $1
LIMIT $3
```

#### Add `memory_get` tool [critical]

Single-memory retrieval by UUID. Required for fetch-before-update and
for inspecting specific memories.

#### Clamp limit/offset values [critical]

```rust
let limit = params.limit.unwrap_or(20).clamp(1, 100);
let offset = params.offset.unwrap_or(0).max(0);
```

### Priority 2: Usability

#### Automatic project name inference

Read a `.memory-project` file from the working directory, or fall
back to the git repository name (`git rev-parse --show-toplevel |
basename`). Provide this as a default in tool descriptions so Claude
Code does not need to guess.

**Complexity: low**

#### Duplicate detection on store

Before inserting, run a similarity search. If a near-duplicate exists
(similarity > 0.92), return it in the response with a suggestion to
update instead of creating a new entry.

**Complexity: low**

#### Full-text search (hybrid mode) [qorvus]

Add a `search_vector TSVECTOR` column with a GIN index. Enhance
`memory_search` with a `mode` parameter: `semantic` (default),
`keyword`, or `hybrid` (union of both, deduplicated). This catches
exact matches that embeddings miss — error codes, function names,
specific identifiers.

**Complexity: medium**

#### Exclude embeddings from list queries

The list path should not fetch the 4 KB embedding vector per row.
Use a SQL projection that omits the `embedding` column, or a
separate `MemorySummary` type without the embedding field.

**Complexity: low**

#### Migration management

Replace manual `psql < migration.sql` with `sqlx::migrate!()`. The
server checks and applies pending migrations on startup, making
deployment zero-touch. Add the `migrate` feature to sqlx.

**Complexity: low** (the estimate of "medium" in the original list
was too high — this is a 10-line change)

### Priority 3: Quality and longevity

#### Enforced rules via memory

Add a `rule` category. A `memory_rules` tool returns all active rules
for a project (no pagination). Designed to be called at session start
via `CLAUDE.md` instruction. This turns the memory server into an
enforceable instruction layer.

**Complexity: low**

#### Auto-save disagreements and consensus

Add a `consensus` category for recording deliberation outcomes. Each
memory stores initial positions, reasoning, and the resolution.
Semantic search naturally surfaces relevant past disagreements.

**Complexity: low**

#### Recency bias in search ranking

Weight search results by `updated_at` recency. A simple approach:
multiply cosine similarity by a time decay factor, e.g.,
`similarity * (1 / (1 + days_since_update * 0.01))`. This degrades
stale memories gracefully without deleting them.

**Complexity: medium**

#### Memory decay and access tracking

Add `access_count INTEGER DEFAULT 0` and `last_accessed TIMESTAMPTZ`
columns. Increment on every search hit. Add a `memory_prune` tool
that lists stale memories (old, zero access count). Optionally, a
decay function that reduces relevance scores over time.

**Complexity: medium**

#### Backup and export

A `memory_export` tool dumps all memories for a project as JSON
(without embeddings). A `memory_import` tool loads a dump and
re-embeds. The export format should be stable and versioned.

**Complexity: low**

### Priority 4: Architecture improvements

#### SQLite backend [qorvus]

Add a SQLite storage backend as an alternative to PostgreSQL. The
qorvus project demonstrates this with a `MemoryStore` trait and two
implementations:

- **SQLite**: stores embeddings as little-endian f32 BLOBs, computes
  cosine similarity in Rust (loads all entries, scores, sorts).
  Zero-config, no external service, suitable for < 50K entries.
- **PostgreSQL**: uses pgvector HNSW for scalable cosine search.

A `MemoryStore` trait abstraction in memory-server would allow the
same pluggability. For most single-user setups, SQLite is sufficient
and eliminates the PostgreSQL dependency entirely.

**Complexity: medium**

#### In-process embeddings (fastembed-rs)

Replace the Ollama HTTP dependency with `fastembed-rs`, which wraps
ONNX Runtime and supports BGE-M3 natively. The model downloads on
first run and runs in-process. This eliminates the Ollama dependency
and makes the binary fully self-contained (with the SQLite backend,
a single binary with zero external services).

**Complexity: medium**

#### Lazy connection with retry

Replace the hard exit on PostgreSQL connection failure with lazy
pool initialization and reconnect handling. The server starts even
if the database is temporarily unavailable, and tools return clear
errors until connectivity is restored.

**Complexity: low**

#### Cross-project memory sharing

Allow memories scoped to `_global` (or a configurable scope field)
so that decisions like "always use rustls" apply across all Rust
projects. Modify search to optionally include global memories.

**Complexity: medium**

#### Embedding model hot-swap

Store the model name alongside each embedding. When the configured
model changes, new memories use the new model. Add a
`memory_reindex` tool for bulk re-embedding.

**Complexity: medium**

### Priority 5: Extended features

#### Local LLM review layer

A `review_changes` tool that sends diffs to a local Ollama model for
independent review. The qorvus project implements a full review
pipeline with configurable reviewer models, up to 3 revision rounds,
and streaming events (`review_start`, `review_feedback`,
`review_approved`). That design could be adapted here.

**Complexity: medium-high**

#### IRC bridge

Separate service bridging an IRC channel to Claude Code. The bot
joins a configured channel, forwards messages to Claude Code (via
API or local model), and relays responses. Needs conversation state,
rate limiting, and auth (respond only to configured nicks/channels).

**Complexity: medium** (separate project)

#### Conversation summarization

Automatically summarize completed Claude Code sessions and store key
decisions, errors, and patterns as memories. Requires conversation
transcript access (Claude Code hooks or post-session trigger) and a
summarization model. Highest-value improvement but hardest to build.

**Complexity: high**

#### Web UI for memory browsing

Web interface for browsing, searching, editing, and managing memories
outside of Claude Code. Use axum alongside the stdio MCP server, or
as a separate binary sharing the database. The qorvus project's full
web UI with SSE streaming could serve as a reference.

**Complexity: high**

#### Multi-tenant / team memory

Per-user and shared team memories with auth and access control.
Fundamentally changes the architecture from single-user local tool
to shared service. Better designed as a separate project.

**Complexity: high** (effective rewrite)
