ALTER TABLE memories
    ADD COLUMN workflow_artifact BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE sessions
    ADD COLUMN workflow_artifact BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE session_logs
    ADD COLUMN workflow_artifact BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE session_log_chunks
    ADD COLUMN workflow_artifact BOOLEAN NOT NULL DEFAULT FALSE;

CREATE INDEX idx_memories_tags ON memories USING gin (tags);

-- Normalize only exact relationship tokens accepted by Rust's UUID parser.
-- Expression indexes preserve case-insensitive UUID identity without scanning
-- or locking unrelated plans/reviews, including legacy UUID spellings.
CREATE OR REPLACE FUNCTION workflow_relationship_ids(tags TEXT[], prefix TEXT)
RETURNS UUID[] LANGUAGE SQL IMMUTABLE STRICT PARALLEL SAFE AS $$
    SELECT ARRAY(
        SELECT CASE
            WHEN value ~ '^[0-9A-Fa-f]{32}$'
              OR value ~ '^[0-9A-Fa-f]{8}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{12}$'
              OR value ~ '^\{[0-9A-Fa-f]{8}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{12}\}$'
              OR value ~ '^urn:uuid:[0-9A-Fa-f]{8}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{12}$'
            THEN replace(value, 'urn:uuid:', '')::UUID
        END
        FROM unnest(tags) AS tag
        CROSS JOIN LATERAL (SELECT substr(tag, length(prefix) + 1) AS value) parsed
        WHERE starts_with(tag, prefix)
    )
$$;

CREATE INDEX IF NOT EXISTS idx_memories_superseded_plan_ids ON memories
    USING gin (workflow_relationship_ids(tags, 'supersedes-plan:'))
    WHERE category = 'plan';
CREATE INDEX IF NOT EXISTS idx_memories_reviewed_item_ids ON memories
    USING gin (workflow_relationship_ids(tags, 'reviewed-item:'))
    WHERE category = 'decision';

WITH RECURSIVE marked_plans(id) AS (
    SELECT id
    FROM memories
    WHERE category = 'plan'
      AND (
          cardinality(array_remove(workflow_relationship_ids(tags, 'task:'), NULL)) > 0
          OR cardinality(array_remove(workflow_relationship_ids(tags, 'superseded-for-task:'), NULL)) > 0
      )
    UNION
    SELECT prior.id
    FROM marked_plans marked
    JOIN memories current ON current.id = marked.id
    JOIN memories prior
      ON workflow_relationship_ids(current.tags, 'supersedes-plan:') @> ARRAY[prior.id]
     AND prior.category = 'plan'
)
UPDATE memories
SET workflow_artifact = TRUE
WHERE workflow_artifact = FALSE
  AND id IN (SELECT id FROM marked_plans);

UPDATE memories AS decision
SET workflow_artifact = TRUE
WHERE decision.workflow_artifact = FALSE
  AND decision.category = 'decision'
  AND EXISTS (
      SELECT 1
      FROM memories AS plan
      WHERE plan.category = 'plan'
        AND plan.workflow_artifact
        AND workflow_relationship_ids(decision.tags, 'reviewed-item:') @> ARRAY[plan.id]
  );

UPDATE sessions AS session
SET workflow_artifact = TRUE
WHERE session.workflow_artifact = FALSE
  AND EXISTS (
      SELECT 1
      FROM session_messages AS message
      WHERE message.session_id = session.id
        AND concat_ws(E'\n', message.content, message.metadata) ~
            '(^|[^[:alnum:]_-])task:[0-9A-Fa-f]{8}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{12}($|[^[:alnum:]_-])'
  );

UPDATE session_logs AS log
SET workflow_artifact = TRUE
WHERE log.workflow_artifact = FALSE
  AND (
      concat_ws(E'\n', log.content, log.summary) ~
          '(^|[^[:alnum:]_-])task:[0-9A-Fa-f]{8}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{12}($|[^[:alnum:]_-])'
      OR EXISTS (
          SELECT 1
          FROM sessions AS session
          WHERE session.external_session_id = log.session_id
            AND session.workflow_artifact
      )
  );

UPDATE sessions AS session
SET workflow_artifact = TRUE
WHERE session.workflow_artifact = FALSE
  AND EXISTS (
      SELECT 1
      FROM session_logs AS log
      WHERE log.session_id = session.external_session_id
        AND log.workflow_artifact
  );

UPDATE session_log_chunks AS chunk
SET workflow_artifact = log.workflow_artifact
FROM session_logs AS log
WHERE chunk.session_log_id = log.id
  AND chunk.workflow_artifact IS DISTINCT FROM log.workflow_artifact;

ALTER TABLE session_logs
    ADD CONSTRAINT session_logs_id_workflow_artifact_key
    UNIQUE (id, workflow_artifact);

ALTER TABLE session_log_chunks
    DROP CONSTRAINT session_log_chunks_session_log_id_fkey;
ALTER TABLE session_log_chunks
    ADD CONSTRAINT session_log_chunks_parent_workflow_artifact_fkey
    FOREIGN KEY (session_log_id, workflow_artifact)
    REFERENCES session_logs (id, workflow_artifact)
    ON UPDATE CASCADE
    ON DELETE CASCADE;
