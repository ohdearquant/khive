//! Tables a pack creates outside the migration chain.
//!
//! A pack's tables are applied to the backend that pack is assigned, so a given
//! SQLite file holds none, some or all of them. The census lists a table only
//! when the file has it, so every step here asks the census first and a file
//! without the table is a no-op rather than an error.

use super::*;

/// Task lifecycle audit rows, written by the gtd pack: `note_id` names a task
/// note and `namespace` is nullable.
const TASK_AUDIT: &str = "gtd_lifecycle_audit";

/// Per-namespace rows that name no subject, like the brain aggregates: they move
/// only when the request is total and single-target. The evaluation runs are
/// summaries and the other three are receipts of work done in the namespace.
/// `exec_events` rows belong to an `exec_runs` row of the same namespace and the
/// step below treats every table here alike, so those two always move or stay
/// together.
const NAMESPACE_SCOPED_PACK_TABLES: &[&str] = &[
    "knowledge_eval_runs",
    "exec_runs",
    "exec_events",
    "git_receipts",
];

/// Authorization state written by the tool pack. A row carried into a target
/// would grant, or deny, there what the target never decided, so no move carries
/// these and no move deletes them.
const POLICY: &str = "tool_policy";
const GRANTS: &str = "tool_grants";

/// Cached ANN index snapshots. The key in the `namespace` column is either a
/// bare namespace or a composite that starts with one.
const SNAPSHOTS: &str = "retrieval_snapshots";

fn present(census: &NamespaceCensus, table: &str) -> bool {
    census
        .tables
        .iter()
        .any(|candidate| candidate.name == table)
}

/// Carry each task's audit rows to the namespace its note is routed to.
///
/// Selected by the routed note kind while the notes are still in the source, so
/// this runs before the route loop. A row whose `note_id` is not a routed source
/// note matches nothing here and stays, and a row with a NULL namespace is
/// outside the source by definition, so `namespace = ?1` never reaches it.
pub(super) fn move_task_audit(
    conn: &Connection,
    census: &NamespaceCensus,
    request: &MoveRequest,
    rows: &mut BTreeMap<String, u64>,
) -> rusqlite::Result<()> {
    if !present(census, TASK_AUDIT) {
        return Ok(());
    }
    let source = request.source.as_str();
    for route in &request.routes {
        let SubjectClass::Note(kind) = &route.class else {
            continue;
        };
        let moved = conn.execute(
            "UPDATE gtd_lifecycle_audit SET namespace = ?2 \
             WHERE namespace = ?1 AND note_id IN (\
               SELECT id FROM notes WHERE namespace = ?1 AND kind = ?3)",
            rusqlite::params![source, route.target.as_str(), kind.as_str()],
        )? as u64;
        *rows.entry(TASK_AUDIT.to_string()).or_default() += moved;
    }
    Ok(())
}

/// What is left to do for these tables once the subjects have moved: report the
/// audit rows that stayed, carry or report the per-namespace rows, leave the
/// authorization rows and report them, and drop the source's index snapshots.
pub(super) fn settle_pack_tables(
    conn: &Connection,
    census: &NamespaceCensus,
    request: &MoveRequest,
    counts: &mut MoveCounts,
) -> rusqlite::Result<()> {
    let source = request.source.as_str();

    if present(census, TASK_AUDIT) {
        let left = count_in_namespace(conn, TASK_AUDIT, source)?;
        if left > 0 {
            counts.left_behind.insert(TASK_AUDIT.to_string(), left);
        }
    }

    for table in NAMESPACE_SCOPED_PACK_TABLES {
        if !present(census, table) {
            continue;
        }
        match request.single_target() {
            Some(target) => {
                move_whole_table(conn, table, source, target, &mut counts.rows)?;
            }
            None => {
                let left = count_in_namespace(conn, table, source)?;
                if left > 0 {
                    counts.left_behind.insert((*table).to_string(), left);
                }
            }
        }
    }

    leave_authorization_behind(conn, census, request, counts)?;

    if present(census, SNAPSHOTS) {
        drop_source_snapshots(conn, source)?;
    }
    Ok(())
}

/// Report the authorization rows a move leaves in the source, and write nothing.
///
/// `left_behind` counts every row that stayed. The two counts on the result are
/// the ones an operator acts on: a policy row is in force until it is
/// soft-deleted, and a grant row while it is granted, not invalidated by a
/// registration and not expired. Both tests restate what the tool pack reads
/// (`deleted_at IS NULL` for a policy; for a grant `status = 'granted'`,
/// `expires_at IS NULL OR expires_at > now` and both invalidation columns
/// NULL) because this crate cannot depend on the pack; `khive-pack-tool` pins
/// them against the pack's own writers and readers. The pack also matches a
/// grant's registry pin against the tool's current registration, which this
/// crate cannot read, so a pinned grant that no longer matches still counts.
///
/// The instant comes from the request, so a test chooses it. Without one it is
/// the wall clock at the call.
fn leave_authorization_behind(
    conn: &Connection,
    census: &NamespaceCensus,
    request: &MoveRequest,
    counts: &mut MoveCounts,
) -> rusqlite::Result<()> {
    let source = request.source.as_str();
    let now_micros = request
        .now_micros
        .unwrap_or_else(|| chrono::Utc::now().timestamp_micros());

    if present(census, POLICY) {
        let left = count_in_namespace(conn, POLICY, source)?;
        if left > 0 {
            counts.left_behind.insert(POLICY.to_string(), left);
            let live = conn.query_row(
                "SELECT COUNT(*) FROM tool_policy WHERE namespace = ?1 AND deleted_at IS NULL",
                [source],
                |row| row.get::<_, i64>(0),
            )? as u64;
            counts.live_policies_left_behind = live;
        }
    }

    if present(census, GRANTS) {
        let left = count_in_namespace(conn, GRANTS, source)?;
        if left > 0 {
            counts.left_behind.insert(GRANTS.to_string(), left);
            let in_force = conn.query_row(
                "SELECT COUNT(*) FROM tool_grants \
                 WHERE namespace = ?1 AND status = 'granted' \
                 AND (expires_at IS NULL OR expires_at > ?2) \
                 AND invalidated_by_registry_id IS NULL AND invalidated_at IS NULL",
                rusqlite::params![source, now_micros],
                |row| row.get::<_, i64>(0),
            )? as u64;
            counts.grants_in_force_left_behind = in_force;
        }
    }

    if counts.live_policies_left_behind > 0 || counts.grants_in_force_left_behind > 0 {
        tracing::warn!(
            namespace = source,
            live_policies = counts.live_policies_left_behind,
            grants_in_force = counts.grants_in_force_left_behind,
            "a namespace move left authorization rows in the source namespace"
        );
    }
    Ok(())
}

/// Delete the snapshots the source namespace owns, and write nothing for the
/// target.
///
/// A snapshot is a cache of an index over the namespace it names, so a move that
/// takes the vectors out leaves it describing a corpus that is gone. Its key is
/// not always a bare namespace: the knowledge Vamana module stores
/// `{namespace}::vamana::{model}`, which the `namespace = ?1` count and the
/// `UPDATE ... WHERE namespace = ?1` form never reach. So the source owns the
/// key equal to it and any key that starts with it followed by `::`.
///
/// `LIKE` is a prefilter only: it is ASCII case-insensitive, so `a::x` would
/// match a namespace `A` that merely differs in case. The `substr` comparison is
/// binary and decides, and the escaping keeps `%`, `_` and the escape character
/// in the namespace itself from changing what the pattern matches.
fn drop_source_snapshots(conn: &Connection, source: &str) -> rusqlite::Result<()> {
    let prefix = format!("{}::%", khive_types::escape_like_literal(source));
    conn.execute(
        "DELETE FROM retrieval_snapshots WHERE namespace = ?1 \
         OR (namespace LIKE ?2 ESCAPE '\\' \
             AND substr(namespace, 1, length(?1) + 2) = ?1 || '::')",
        rusqlite::params![source, prefix],
    )?;
    Ok(())
}
