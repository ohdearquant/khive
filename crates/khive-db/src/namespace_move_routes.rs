use super::knowledge_move::MIRROR_EXISTS;
use super::*;

/// Resolve logical source subjects. A domain's same-ID mirror and sections
/// follow the domain route, while ordinary atoms follow the atom route.
pub(super) fn routed_subjects(request: &MoveRequest) -> (String, Vec<rusqlite::types::Value>) {
    let mut parameters = vec![request.source.clone().into()];
    let mut values = Vec::new();
    for route in &request.routes {
        let (class, kind) = match &route.class {
            SubjectClass::Note(kind) => ("note", Some(kind)),
            SubjectClass::Entity(kind) => ("entity", Some(kind)),
            SubjectClass::Edge => ("edge", None),
            SubjectClass::Atom => ("atom", None),
            SubjectClass::Domain => ("domain", None),
        };
        let first = parameters.len() + 1;
        parameters.push(class.to_owned().into());
        parameters.push(kind.map_or(rusqlite::types::Value::Null, |kind| kind.clone().into()));
        parameters.push(route.target.clone().into());
        values.push(format!("(?{first}, ?{}, ?{})", first + 1, first + 2));
    }
    if values.is_empty() {
        return (
            "SELECT NULL AS subject_id, NULL AS target WHERE ?1 IS NULL AND 0".to_owned(),
            parameters,
        );
    }
    // VALUES rows are not compound SELECT terms. The six subject/section
    // branches stay constant even when a caller routes hundreds of kinds.
    let selector = format!(
        "WITH route_data(class, kind, target) AS (VALUES {}) \
         SELECT subject.id AS subject_id, route.target FROM notes AS subject \
         JOIN route_data AS route ON route.class = 'note' AND subject.kind = route.kind \
         WHERE subject.namespace = ?1 \
         UNION ALL SELECT subject.id, route.target FROM entities AS subject \
         JOIN route_data AS route ON route.class = 'entity' AND subject.kind = route.kind \
         WHERE subject.namespace = ?1 \
         UNION ALL SELECT subject.id, route.target FROM graph_edges AS subject \
         JOIN route_data AS route ON route.class = 'edge' WHERE subject.namespace = ?1 \
         UNION ALL SELECT atom.id, route.target FROM knowledge_atoms AS atom \
         JOIN route_data AS route ON route.class = 'atom' \
         WHERE atom.namespace = ?1 AND NOT {MIRROR_EXISTS} \
         UNION ALL SELECT domain.id, route.target FROM knowledge_domains AS domain \
         JOIN route_data AS route ON route.class = 'domain' WHERE domain.namespace = ?1 \
         UNION ALL SELECT section.id, route.target FROM knowledge_sections AS section \
         JOIN knowledge_atoms AS atom ON atom.id = section.atom_id \
         JOIN route_data AS route ON route.class = \
             CASE WHEN {MIRROR_EXISTS} THEN 'domain' ELSE 'atom' END \
         WHERE section.namespace = ?1 AND atom.namespace = ?1",
        values.join(", ")
    );
    (selector, parameters)
}
