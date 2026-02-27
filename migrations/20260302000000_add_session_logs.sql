CREATE TABLE session_logs (
    id          UUID PRIMARY KEY,
    content     TEXT NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    cwd         TEXT NOT NULL DEFAULT '',
    embedding   VECTOR(1024) NOT NULL,
    project     TEXT NOT NULL DEFAULT '',
    session_id  TEXT NOT NULL UNIQUE,
    summary     TEXT NOT NULL
);

CREATE INDEX idx_session_logs_embedding
    ON session_logs USING hnsw (embedding vector_cosine_ops);
CREATE INDEX idx_session_logs_project ON session_logs (project);

ALTER TABLE session_logs ADD COLUMN fts tsvector
    GENERATED ALWAYS AS (
        setweight(immutable_to_tsvector('english'::regconfig, summary), 'A') ||
        setweight(immutable_to_tsvector('english'::regconfig, content), 'B')
    ) STORED;

CREATE INDEX idx_session_logs_fts ON session_logs USING gin (fts);
