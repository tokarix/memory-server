ALTER TABLE memories ADD COLUMN fts tsvector
    GENERATED ALWAYS AS (
        setweight(to_tsvector('english', summary), 'A') ||
        setweight(to_tsvector('english', content), 'B') ||
        setweight(to_tsvector('english', array_to_string(tags, ' ')), 'C')
    ) STORED;

CREATE INDEX idx_memories_fts ON memories USING gin (fts);
