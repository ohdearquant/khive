use super::*;

pub(super) const MIRROR_EXISTS: &str = "EXISTS (SELECT 1 FROM knowledge_domains AS domain \
     WHERE domain.id = atom.id AND domain.namespace = atom.namespace)";

pub(super) fn ordinary_atom_count(conn: &Connection, source: &str) -> rusqlite::Result<u64> {
    conn.query_row(
        &format!("SELECT COUNT(*) FROM knowledge_atoms AS atom WHERE atom.namespace = ?1 AND NOT {MIRROR_EXISTS}"),
        [source],
        |row| row.get::<_, i64>(0),
    )
    .map(|count| count as u64)
}

pub(super) fn move_knowledge_atoms(
    conn: &Connection,
    request: &MoveRequest,
    target: &str,
    mirrors: bool,
    rows: &mut BTreeMap<String, u64>,
) -> rusqlite::Result<u64> {
    let source = request.source.as_str();
    let predicate = if mirrors {
        MIRROR_EXISTS.to_owned()
    } else {
        format!("NOT {MIRROR_EXISTS}")
    };
    if request.single_target().is_none() {
        let sections = conn.execute(
            &format!(
                "UPDATE knowledge_sections SET namespace = ?2 WHERE namespace = ?1 \
             AND atom_id IN (SELECT atom.id FROM knowledge_atoms AS atom \
                             WHERE atom.namespace = ?1 AND {predicate})"
            ),
            rusqlite::params![source, target],
        )? as u64;
        *rows.entry("knowledge_sections".into()).or_default() += sections;
    }
    let moved = conn.execute(
        &format!(
            "UPDATE knowledge_atoms AS atom SET namespace = ?2 \
             WHERE atom.namespace = ?1 AND {predicate}"
        ),
        rusqlite::params![source, target],
    )? as u64;
    *rows.entry("knowledge_atoms".into()).or_default() += moved;
    Ok(moved)
}
