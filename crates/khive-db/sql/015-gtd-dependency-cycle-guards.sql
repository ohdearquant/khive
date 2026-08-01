-- V15: Commit-time cycle guards for GTD task dependencies.
--
-- The GTD pack performs typed, bounded reachability checks before canonical
-- writes. These triggers are the transaction-time backstop shared by direct
-- storage writes, concurrent requests, and kkernel's multi-op atomic executor.
-- They are deliberately narrow: only live `task` note properties and live
-- task-to-task `depends_on` edges are governed. Other note kinds, relations,
-- and entity dependency graphs retain their existing behavior.
--
-- Typed update paths require lowercase hyphenated UUIDs. The note triggers
-- still normalize case, hyphens, braces, and the `urn:uuid:` prefix while
-- comparing keys so a direct-storage writer cannot evade the durable guard
-- with another spelling accepted by `Uuid::parse_str`. Recursive expansion
-- is array-only; malformed legacy scalar/object values are not edges.

CREATE TRIGGER IF NOT EXISTS gtd_task_dependency_cycle_notes_bi
BEFORE INSERT ON notes
WHEN NEW.kind = 'task'
    AND NEW.deleted_at IS NULL
    AND json_type(NEW.properties, '$.depends_on') = 'array'
BEGIN
    SELECT CASE WHEN EXISTS (
        WITH RECURSIVE dependency_walk(id) AS (
            SELECT lower(replace(replace(replace(
                CASE
                    WHEN lower(value) LIKE 'urn:uuid:%' THEN substr(value, 10)
                    ELSE value
                END,
                '-', ''), '{', ''), '}', ''))
            FROM json_each(NEW.properties, '$.depends_on')
            WHERE type = 'text'
            UNION
            SELECT lower(replace(replace(replace(
                CASE
                    WHEN lower(dependency.value) LIKE 'urn:uuid:%'
                        THEN substr(dependency.value, 10)
                    ELSE dependency.value
                END,
                '-', ''), '{', ''), '}', ''))
            FROM dependency_walk AS walk
            JOIN notes AS task
                ON lower(replace(replace(replace(
                    CASE
                        WHEN lower(task.id) LIKE 'urn:uuid:%' THEN substr(task.id, 10)
                        ELSE task.id
                    END,
                    '-', ''), '{', ''), '}', '')) = walk.id
                AND task.namespace = NEW.namespace
                AND task.kind = 'task'
                AND task.deleted_at IS NULL
            JOIN json_each(task.properties, '$.depends_on') AS dependency
                ON json_type(task.properties, '$.depends_on') = 'array'
                AND dependency.type = 'text'
        )
        SELECT 1
        FROM dependency_walk
        WHERE id = lower(replace(replace(replace(
            CASE
                WHEN lower(NEW.id) LIKE 'urn:uuid:%' THEN substr(NEW.id, 10)
                ELSE NEW.id
            END,
            '-', ''), '{', ''), '}', ''))
    ) THEN RAISE(ABORT, 'task properties.depends_on dependency cycle') END;
END;

CREATE TRIGGER IF NOT EXISTS gtd_task_dependency_cycle_notes_bu
BEFORE UPDATE OF id, namespace, kind, properties, deleted_at ON notes
WHEN NEW.kind = 'task'
    AND NEW.deleted_at IS NULL
    AND json_type(NEW.properties, '$.depends_on') = 'array'
BEGIN
    SELECT CASE WHEN EXISTS (
        WITH RECURSIVE dependency_walk(id) AS (
            SELECT lower(replace(replace(replace(
                CASE
                    WHEN lower(value) LIKE 'urn:uuid:%' THEN substr(value, 10)
                    ELSE value
                END,
                '-', ''), '{', ''), '}', ''))
            FROM json_each(NEW.properties, '$.depends_on')
            WHERE type = 'text'
            UNION
            SELECT lower(replace(replace(replace(
                CASE
                    WHEN lower(dependency.value) LIKE 'urn:uuid:%'
                        THEN substr(dependency.value, 10)
                    ELSE dependency.value
                END,
                '-', ''), '{', ''), '}', ''))
            FROM dependency_walk AS walk
            JOIN notes AS task
                ON lower(replace(replace(replace(
                    CASE
                        WHEN lower(task.id) LIKE 'urn:uuid:%' THEN substr(task.id, 10)
                        ELSE task.id
                    END,
                    '-', ''), '{', ''), '}', '')) = walk.id
                AND task.namespace = NEW.namespace
                AND task.kind = 'task'
                AND task.deleted_at IS NULL
            JOIN json_each(task.properties, '$.depends_on') AS dependency
                ON json_type(task.properties, '$.depends_on') = 'array'
                AND dependency.type = 'text'
        )
        SELECT 1
        FROM dependency_walk
        WHERE id = lower(replace(replace(replace(
            CASE
                WHEN lower(NEW.id) LIKE 'urn:uuid:%' THEN substr(NEW.id, 10)
                ELSE NEW.id
            END,
            '-', ''), '{', ''), '}', ''))
    ) THEN RAISE(ABORT, 'task properties.depends_on dependency cycle') END;
END;

CREATE TRIGGER IF NOT EXISTS gtd_task_dependency_cycle_edges_bi
BEFORE INSERT ON graph_edges
WHEN NEW.relation = 'depends_on'
    AND NEW.deleted_at IS NULL
    AND EXISTS (
        SELECT 1
        FROM notes
        WHERE id = NEW.source_id
            AND kind = 'task'
            AND deleted_at IS NULL
    )
    AND EXISTS (
        SELECT 1
        FROM notes
        WHERE id = NEW.target_id
            AND kind = 'task'
            AND deleted_at IS NULL
    )
BEGIN
    SELECT CASE WHEN EXISTS (
        WITH RECURSIVE dependency_walk(id) AS (
            SELECT NEW.target_id
            UNION
            SELECT edge.target_id
            FROM dependency_walk AS walk
            JOIN graph_edges AS edge
                ON edge.source_id = walk.id
                AND edge.namespace = NEW.namespace
                AND edge.relation = 'depends_on'
                AND edge.deleted_at IS NULL
                AND NOT (
                    edge.namespace = NEW.namespace
                    AND edge.id = NEW.id
                )
            JOIN notes AS source_task
                ON source_task.id = edge.source_id
                AND source_task.kind = 'task'
                AND source_task.deleted_at IS NULL
            JOIN notes AS target_task
                ON target_task.id = edge.target_id
                AND target_task.kind = 'task'
                AND target_task.deleted_at IS NULL
        )
        SELECT 1
        FROM dependency_walk
        WHERE id = NEW.source_id
    ) THEN RAISE(ABORT, 'task depends_on edge dependency cycle') END;
END;

CREATE TRIGGER IF NOT EXISTS gtd_task_dependency_cycle_edges_bu
BEFORE UPDATE OF namespace, id, source_id, target_id, relation, deleted_at ON graph_edges
WHEN NEW.relation = 'depends_on'
    AND NEW.deleted_at IS NULL
    AND EXISTS (
        SELECT 1
        FROM notes
        WHERE id = NEW.source_id
            AND kind = 'task'
            AND deleted_at IS NULL
    )
    AND EXISTS (
        SELECT 1
        FROM notes
        WHERE id = NEW.target_id
            AND kind = 'task'
            AND deleted_at IS NULL
    )
BEGIN
    SELECT CASE WHEN EXISTS (
        WITH RECURSIVE dependency_walk(id) AS (
            SELECT NEW.target_id
            UNION
            SELECT edge.target_id
            FROM dependency_walk AS walk
            JOIN graph_edges AS edge
                ON edge.source_id = walk.id
                AND edge.namespace = NEW.namespace
                AND edge.relation = 'depends_on'
                AND edge.deleted_at IS NULL
                AND NOT (
                    edge.namespace = OLD.namespace
                    AND edge.id = OLD.id
                )
            JOIN notes AS source_task
                ON source_task.id = edge.source_id
                AND source_task.kind = 'task'
                AND source_task.deleted_at IS NULL
            JOIN notes AS target_task
                ON target_task.id = edge.target_id
                AND target_task.kind = 'task'
                AND target_task.deleted_at IS NULL
        )
        SELECT 1
        FROM dependency_walk
        WHERE id = NEW.source_id
    ) THEN RAISE(ABORT, 'task depends_on edge dependency cycle') END;
END;
