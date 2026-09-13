CREATE TRIGGER IF NOT EXISTS tool_grants_invalidate_on_registry_insert
AFTER INSERT ON entities
WHEN NEW.deleted_at IS NULL
    AND NEW.kind = 'project'
    AND EXISTS (SELECT 1 FROM json_each(NEW.tags) WHERE lower(value) = 'tool-registry')
BEGIN
    UPDATE tool_grants
    SET invalidated_by_registry_id = NEW.id,
        invalidated_at = NEW.created_at
    WHERE namespace = NEW.namespace
        AND status = 'granted'
        AND registry_id IS NULL
        AND definition_digest IS NULL
        AND invalidated_by_registry_id IS NULL
        AND invalidated_at IS NULL
        AND (
            CAST(lower(tool) AS BLOB) = CAST(lower(NEW.name) AS BLOB)
            OR tool = '*'
            OR (
                substr(CAST(tool AS BLOB), -1) = X'2A'
                AND substr(CAST(NEW.name AS BLOB), 1, length(CAST(tool AS BLOB)) - 1)
                    = substr(CAST(tool AS BLOB), 1, length(CAST(tool AS BLOB)) - 1)
            )
        );
END;

-- Old rows cannot identify deleted registrations; preserve the earliest
-- matching live registry evidence without treating it as an approval.
WITH first_known_registry AS (
    SELECT
        grants.id AS grant_id,
        registry.id AS registry_id,
        registry.created_at AS registered_at,
        ROW_NUMBER() OVER (
            PARTITION BY grants.id
            ORDER BY (CAST(lower(grants.tool) AS BLOB) = CAST(lower(registry.name) AS BLOB)) DESC,
                registry.created_at, registry.id
        ) AS ordinal
    FROM tool_grants AS grants
    JOIN entities AS registry ON registry.namespace = grants.namespace
        AND registry.deleted_at IS NULL
        AND registry.kind = 'project'
        AND EXISTS (SELECT 1 FROM json_each(registry.tags) WHERE lower(value) = 'tool-registry')
        AND (
            CAST(lower(grants.tool) AS BLOB) = CAST(lower(registry.name) AS BLOB)
            OR grants.tool = '*'
            OR (
                substr(CAST(grants.tool AS BLOB), -1) = X'2A'
                AND substr(CAST(registry.name AS BLOB), 1, length(CAST(grants.tool AS BLOB)) - 1)
                    = substr(CAST(grants.tool AS BLOB), 1, length(CAST(grants.tool AS BLOB)) - 1)
            )
        )
    WHERE grants.status = 'granted'
        AND grants.registry_id IS NULL
        AND grants.definition_digest IS NULL
        AND grants.invalidated_by_registry_id IS NULL
        AND grants.invalidated_at IS NULL
)
UPDATE tool_grants
SET invalidated_by_registry_id = first_known_registry.registry_id,
    invalidated_at = first_known_registry.registered_at
FROM first_known_registry
WHERE tool_grants.id = first_known_registry.grant_id
    AND first_known_registry.ordinal = 1;
