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

/// Per-namespace summary rows that name no subject, like the brain aggregates:
/// they move only when the request is total and single-target.
const NAMESPACE_SCOPED_PACK_TABLES: &[&str] = &["knowledge_eval_runs"];

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
/// audit rows that stayed, carry or report the summary rows, and drop the
/// source's index snapshots.
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

    if present(census, SNAPSHOTS) {
        drop_source_snapshots(conn, source)?;
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
