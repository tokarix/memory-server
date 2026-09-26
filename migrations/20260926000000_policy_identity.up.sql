ALTER TABLE memories
    ADD COLUMN policy_key TEXT,
    ADD COLUMN policy_revision BIGINT,
    ADD COLUMN policy_delivery_class TEXT,
    ADD COLUMN policy_state TEXT,
    ADD COLUMN policy_supersedes UUID;

ALTER TABLE memories
    ADD CONSTRAINT memories_policy_shape CHECK (
        (
            policy_key IS NULL
            AND policy_revision IS NULL
            AND policy_delivery_class IS NULL
            AND policy_state IS NULL
            AND policy_supersedes IS NULL
        )
        OR (
            policy_key IS NOT NULL
            AND policy_revision IS NOT NULL
            AND policy_delivery_class IS NOT NULL
            AND policy_state IS NOT NULL
            AND category = 'rule'
            AND octet_length(policy_key) BETWEEN 1 AND 128
            AND policy_key COLLATE "C" ~ '^[a-z0-9][a-z0-9._:-]*$'
            AND policy_revision > 0
            AND policy_delivery_class IN ('contextual', 'mandatory')
            AND policy_state IN ('active', 'superseded')
            AND policy_supersedes IS DISTINCT FROM id
        )
    );

ALTER TABLE memories
    ADD CONSTRAINT memories_policy_revision_unique
    UNIQUE (project, policy_key, policy_revision);

ALTER TABLE memories
    ADD CONSTRAINT memories_policy_identity_reference_unique
    UNIQUE (id, project, policy_key);

ALTER TABLE memories
    ADD CONSTRAINT memories_policy_predecessor_fk
    FOREIGN KEY (policy_supersedes, project, policy_key)
    REFERENCES memories (id, project, policy_key)
    ON DELETE RESTRICT;

CREATE UNIQUE INDEX memories_policy_one_active
    ON memories (project, policy_key)
    WHERE policy_state = 'active';
