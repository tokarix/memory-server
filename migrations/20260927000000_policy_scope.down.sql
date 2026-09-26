DO $$
BEGIN
    IF EXISTS (
        SELECT 1 FROM memories
        WHERE policy_selectors <> '{}'::jsonb OR policy_values <> '{}'::jsonb
    ) THEN
        RAISE EXCEPTION 'cannot discard classified policy selectors or values';
    END IF;
END
$$;

ALTER TABLE memories DROP CONSTRAINT memories_policy_scope_shape;
ALTER TABLE memories DROP COLUMN policy_selectors;
ALTER TABLE memories DROP COLUMN policy_values;
