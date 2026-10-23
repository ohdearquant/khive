WITH latest AS (
    SELECT commit_order, policy_domain, namespace, run_id, view_bytes,
        row_number() OVER (
            PARTITION BY policy_domain, run_id
            ORDER BY commit_order DESC
        ) AS row_rank
    FROM charter_waiting_changes
    WHERE policy_domain = ?1
        AND namespace IN (SELECT value FROM json_each(?2))
),
selected AS MATERIALIZED (
    SELECT commit_order, view_bytes
    FROM latest
    WHERE row_rank = 1
        AND (
            (?5 IS NULL AND json_extract(view_bytes, '$.state') = 'open')
            OR (?5 IS NOT NULL AND commit_order > ?5)
        )
        AND (?3 IS NULL OR json_extract(view_bytes, '$.charter') = ?3)
        AND (?4 IS NULL OR json_extract(view_bytes, '$.subject.kind') = ?4)
),
cursor AS (
    SELECT coalesce(max(commit_order), 0) AS commit_order
    FROM charter_waiting_changes
)
SELECT 0 AS cursor_only, selected.view_bytes, cursor.commit_order AS cursor_order
FROM selected CROSS JOIN cursor
UNION ALL
SELECT 1 AS cursor_only, NULL AS view_bytes, cursor.commit_order AS cursor_order
FROM cursor
WHERE NOT EXISTS (SELECT 1 FROM selected)
