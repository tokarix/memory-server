-- Immutable wrappers for STABLE/non-immutable functions.
-- Required for use in GENERATED ALWAYS AS expressions (PG 18+ enforces this).
CREATE OR REPLACE FUNCTION immutable_array_to_string(arr text[], sep text)
RETURNS text LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$
    SELECT array_to_string(arr, sep);
$$;

CREATE OR REPLACE FUNCTION immutable_to_tsvector(config regconfig, input text)
RETURNS tsvector LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$
    SELECT to_tsvector(config, input);
$$;

ALTER TABLE memories ADD COLUMN fts tsvector
    GENERATED ALWAYS AS (
        setweight(immutable_to_tsvector('english'::regconfig, summary), 'A') ||
        setweight(immutable_to_tsvector('english'::regconfig, content), 'B') ||
        setweight(immutable_to_tsvector('english'::regconfig, immutable_array_to_string(tags, ' ')), 'C')
    ) STORED;

CREATE INDEX idx_memories_fts ON memories USING gin (fts);
