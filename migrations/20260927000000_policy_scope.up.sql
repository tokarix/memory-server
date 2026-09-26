ALTER TABLE memories
    ADD COLUMN policy_selectors JSONB NOT NULL DEFAULT '{}'::jsonb,
    ADD COLUMN policy_values JSONB NOT NULL DEFAULT '{}'::jsonb;

ALTER TABLE memories
    ADD CONSTRAINT memories_policy_scope_shape CHECK (
        jsonb_typeof(policy_selectors) = 'object'
        AND jsonb_typeof(policy_values) = 'object'
        AND (policy_selectors - ARRAY['profile', 'phase', 'language', 'tool']) = '{}'::jsonb
        AND (NOT (policy_selectors ? 'profile') OR jsonb_typeof(policy_selectors->'profile') = 'array')
        AND (NOT (policy_selectors ? 'phase') OR jsonb_typeof(policy_selectors->'phase') = 'array')
        AND (NOT (policy_selectors ? 'language') OR jsonb_typeof(policy_selectors->'language') = 'array')
        AND (NOT (policy_selectors ? 'tool') OR jsonb_typeof(policy_selectors->'tool') = 'array')
        AND NOT jsonb_path_exists(policy_selectors, '$.*[*] ? (@.type() != "string")')
        AND NOT jsonb_path_exists(policy_values, '$.* ? (@.type() != "string")')
        AND (policy_key IS NOT NULL OR
             (policy_selectors = '{}'::jsonb AND policy_values = '{}'::jsonb))
    );
