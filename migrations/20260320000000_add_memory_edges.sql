CREATE TYPE edge_relation AS ENUM ('references', 'related_tag', 'similar');

CREATE TYPE edge_origin AS ENUM (
    'content_uuid_ref',
    'embedding_neighbor',
    'manual',
    'shared_tag',
    'structural_tag_ref',
    'usage_reinforcement'
);

CREATE TABLE memory_edges (
    id          UUID PRIMARY KEY,
    src_id      UUID NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    dst_id      UUID NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    src_project TEXT NOT NULL,
    dst_project TEXT NOT NULL,
    relation    edge_relation NOT NULL,
    origin      edge_origin NOT NULL,
    weight      DOUBLE PRECISION NOT NULL DEFAULT 1.0,
    confidence  DOUBLE PRECISION NOT NULL DEFAULT 1.0,
    evidence    TEXT,
    suppressed  BOOLEAN NOT NULL DEFAULT FALSE,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE UNIQUE INDEX idx_memory_edges_upsert
    ON memory_edges (src_id, dst_id, relation, origin);

CREATE INDEX idx_memory_edges_src ON memory_edges (src_id) WHERE NOT suppressed;
CREATE INDEX idx_memory_edges_dst ON memory_edges (dst_id) WHERE NOT suppressed;
CREATE INDEX idx_memory_edges_src_project ON memory_edges (src_project) WHERE NOT suppressed;
CREATE INDEX idx_memory_edges_dst_project ON memory_edges (dst_project) WHERE NOT suppressed;
