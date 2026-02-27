CREATE TABLE session_log_chunks (
    id              UUID PRIMARY KEY,
    chunk_index     INT NOT NULL,
    content         TEXT NOT NULL,
    embedding       VECTOR(1024) NOT NULL,
    session_log_id  UUID NOT NULL REFERENCES session_logs(id) ON DELETE CASCADE,
    UNIQUE (session_log_id, chunk_index)
);

CREATE INDEX idx_session_log_chunks_embedding
    ON session_log_chunks USING hnsw (embedding vector_cosine_ops);
CREATE INDEX idx_session_log_chunks_session_log_id
    ON session_log_chunks (session_log_id);
