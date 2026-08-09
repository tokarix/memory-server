# Workflow artifact verification

PR #61 preserves historical workflow artifacts and excludes them only from
semantic retrieval by default. This records the review fixes and the local
verification environment, rather than production performance guarantees.

## Relationship writes

Plan/Decision insertion and tag replacement hold the provenance advisory
mutex before row locks. Propagation locks the changed row, referenced plan
ancestors, then dependent Decisions. It no longer loads or locks every Plan
and Decision. Positive UUID-array lookups use two category-specific GIN
expression indexes. Their parser accepts the same UUID spellings as the
Rust relationship classifier, including uppercase, simple, braced, and URN
forms; it rejects malformed suffixes and prefix lookalikes.

The reverse ancestor lookup handles a missing ancestor inserted after its
marked successor. Traversal also follows new edges added to an already
marked plan, terminates on cycles, and never clears stored provenance.
Relationship identity is global across projects. Tests hold unrelated Plan
and Decision rows locked while inserts and tag replacements complete under
a bounded timeout, proving those rows do not participate in propagation.

Content-only updates do not change relationship tags or provenance and keep
the existing fast path. Category is immutable through the application and
its update SQL, so the category preflight remains safe; the provenance
transaction reloads the primary row before tag replacement.

## Review submission and bootstrap

`submit_review` already calls `store_memory`, which routes Decision inserts
through `insert_with_workflow_provenance` and builds graph edges after the
transaction commits. The regression now checks the persisted provenance of
an actual submitted review targeting a marked plan, as well as its exact
`review` and `reviewed-item:<uuid>` tags.

Bootstrap intentionally excludes every Plan and every Decision carrying
the `review` tag, including ordinary plan reviews. Existing bootstrap tests
cover that stronger policy and preserve other categories carrying the same
tag. Standalone recall retains its precise workflow-artifact opt-in.

## Search query plans

Local comparison used PostgreSQL 18.4, pgvector 0.8.0, 4,000 memories
(half Plans and half Decisions, two thirds marked), and 1,000 correlated
logs/chunks. Embeddings were identical nonzero 1,024-dimensional vectors to
exercise tied candidate windows; one percent of rows matched the FTS query.
All tables were analyzed. EXPLAIN (ANALYZE, BUFFERS) used the actual default
and inclusive hybrid/session SQL, a candidate limit of 10, and no planner
forcing. Each query includes both its vector and FTS candidate stages.

| Lookup | Selected plan before experimental search indexes |
| --- | --- |
| Incoming plan relationship | Bitmap scan on `idx_memories_superseded_plan_ids` |
| Dependent reviews | Bitmap scan on `idx_memories_reviewed_item_ids` |
| Memory vector, both policies | Sequential scan, filter, sort, window, limit |
| Memory FTS, both policies | Bitmap scan on `idx_memories_fts`, filter, sort, window, limit |
| Session vector, inclusive | Chunk HNSW scan with parent primary-key lookup |
| Session vector, default | Filtered chunk/parent scans, hash join, sort, limit |
| Session FTS, both policies | Parent scan, filter, sort, window, limit |

The relationship lookups each returned one matching row, using their
expression indexes without disabling sequential scans (0.078 ms for the
ancestor lookup and 0.102 ms for dependent reviews in the first run).

Four disposable partial indexes were then added using the literal
`workflow_artifact = FALSE` predicate: memory/chunk HNSW and memory/log FTS
GIN. PostgreSQL selected the two partial FTS indexes for default queries,
but neither partial HNSW index. Inclusive plans remained unchanged. Default
memory query time was 17.686 ms before and 19.195 ms after; default session
query time was 4.783 ms before and 5.037 ms after. These single-run timings
on tied synthetic fixtures do not establish production performance. No
additional default-search index is retained without demonstrated benefit.
The positive relationship indexes are retained for targeted propagation;
they are not claimed to accelerate negative workflow filtering.

## Verification scope

Rust 1.97.1 was used with disk-backed Cargo targets and compiler temporary
files. An unrestricted parallel test run at the recall commit hit the
existing one-second fast-path test timeout (91 other tests passed). The
same test passed at earlier commits; final verification uses four test
workers to bound PostgreSQL fixture contention without relaxing assertions.
Required checks are formatting, all-feature/all-target build and
tests with PostgreSQL/pgvector/migrations enabled, and pedantic Clippy with
warnings denied. All six commits passed these checks independently; the
previous head passed 146 tests. SQLx coverage includes absent-session correlation races,
both append/publication and append/finalization orderings, rollback,
composite parent/chunk constraints, monotonic plan/review propagation,
migration backfill idempotence, and dream mutation rechecks.

The application search regression uses nonzero embedding mocks and actual
storage/review submission. It verifies omitted, false, and true workflow
options with and without a Plan category filter, through graph expansion.
Authoritative list/get/public-neighbor/review/session browsing and bootstrap
retain their separate existing contracts.

The external-session advisory key remains the plan's stable namespaced
64-bit hash. Collisions only serialize unrelated sessions; they do not
weaken provenance. The proposed two-bigint advisory-lock overload does not
exist in PostgreSQL, and splitting the hash into two 32-bit arguments would
not increase its key space. Prepared chunks continue to receive their
resolved parent provenance only inside atomic publication, as documented
at their construction site. `append_session_message` returns an internal
`Option` for a session disappearing before its locked reload; the application
maps `None` to its existing not-found response without changing public DTOs.
UUID validation intentionally does not impose version/variant restrictions
beyond successful parsing.

## September review corrections

The migration now shares `workflow_relationship_ids` with runtime
relationship queries for direct task/legacy markers, ancestor traversal,
and linked reviews. Historical fixtures cover canonical, uppercase,
simple, braced, and URN UUIDs through both direct prefixes, cross-project
ancestor reviews, malformed tags, and an idempotent second backfill.

The stale-absence regression pauses raw preparation at the publisher
boundary using two one-shot barriers. It records absence, waits for both
normalized creation and append to commit with no log, then resumes the
publisher. Both task-marked and ordinary schedules run under a timeout.
A separate bounded test appends a task marker after normalized finalization
has committed, proving promotion of the session, log, and existing chunks
while preserving the finalization timestamp.

HTTP coverage checks the actual recall client's outgoing requests for
omitted, false, and true policy values, including project path escaping.
The API's query extractor tests those values and rejects malformed booleans.
Bootstrap deliberately follows the approved category-specific contract:
non-Decision memories tagged `review` are eligible. This relaxes the older
tag-only exclusion. Normalized session `updated_at` now includes append
activity even for historical messages; the HTTP documentation records this.
