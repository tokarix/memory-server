CREATE TABLE sessions (
    id                  UUID PRIMARY KEY,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    ended_at            TIMESTAMPTZ,
    cwd                 TEXT NOT NULL DEFAULT '',
    project             TEXT NOT NULL DEFAULT '',
    external_session_id TEXT NOT NULL UNIQUE,
    agent               TEXT NOT NULL DEFAULT ''
);

CREATE INDEX idx_sessions_project ON sessions (project);
CREATE INDEX idx_sessions_updated_at ON sessions (updated_at DESC);

CREATE TABLE session_messages (
    id          UUID PRIMARY KEY,
    session_id  UUID NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    agent       TEXT NOT NULL DEFAULT '',
    role        TEXT NOT NULL,
    kind        TEXT NOT NULL DEFAULT 'message',
    content     TEXT NOT NULL,
    metadata    TEXT
);

CREATE INDEX idx_session_messages_session_id
    ON session_messages (session_id, created_at, id);
