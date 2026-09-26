DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM memories WHERE policy_key IS NOT NULL) THEN
        RAISE EXCEPTION
            'cannot downgrade policy identity after classification; export or restore classified history first';
    END IF;
END
$$;

DROP INDEX memories_policy_one_active;

ALTER TABLE memories
    DROP CONSTRAINT memories_policy_predecessor_fk,
    DROP CONSTRAINT memories_policy_identity_reference_unique,
    DROP CONSTRAINT memories_policy_revision_unique,
    DROP CONSTRAINT memories_policy_shape,
    DROP COLUMN policy_supersedes,
    DROP COLUMN policy_state,
    DROP COLUMN policy_delivery_class,
    DROP COLUMN policy_revision,
    DROP COLUMN policy_key;
