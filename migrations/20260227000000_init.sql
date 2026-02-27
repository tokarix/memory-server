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

CREATE INDEX idx_memories_embedding
    ON memories USING hnsw (embedding vector_cosine_ops);
CREATE INDEX idx_memories_category ON memories (category);
CREATE INDEX idx_memories_project ON memories (project);
CREATE INDEX idx_memories_project_category ON memories (project, category);
