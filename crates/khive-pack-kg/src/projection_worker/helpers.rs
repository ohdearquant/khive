//! SQL helpers for the proposals projection worker.

use khive_storage::{
    event::Event,
    types::{SqlStatement, SqlValue},
};

/// Build a conditional event INSERT `SqlStatement` for use in `execute_batch`.
pub(crate) fn build_conditional_event_insert(
    event: &Event,
    guard_sql: &str,
    guard_params: Vec<SqlValue>,
) -> SqlStatement {
    let substrate_str = event.substrate.name().to_string();
    let kind_str = event.kind.name().to_string();
    let outcome_str = event.outcome.name().to_string();
    let payload_str = event.payload.to_string();

    let guard_offset = 17usize;
    let remapped_guard = remap_guard_params(guard_sql, guard_offset);

    let mut params = vec![
        SqlValue::Text(event.id.to_string()),
        SqlValue::Text(event.namespace.clone()),
        SqlValue::Text(event.verb.clone()),
        SqlValue::Text(substrate_str),
        SqlValue::Text(event.actor.clone()),
        SqlValue::Text(kind_str),
        SqlValue::Text(outcome_str),
        SqlValue::Text(payload_str),
        SqlValue::Integer(event.payload_schema_version as i64),
        match event.profile_state_version {
            Some(v) => SqlValue::Integer(v as i64),
            None => SqlValue::Null,
        },
        SqlValue::Integer(event.duration_us),
        match event.target_id {
            Some(u) => SqlValue::Text(u.to_string()),
            None => SqlValue::Null,
        },
        match event.session_id {
            Some(u) => SqlValue::Text(u.to_string()),
            None => SqlValue::Null,
        },
        match &event.aggregate_kind {
            Some(s) => SqlValue::Text(s.clone()),
            None => SqlValue::Null,
        },
        match event.aggregate_id {
            Some(u) => SqlValue::Text(u.to_string()),
            None => SqlValue::Null,
        },
        SqlValue::Integer(event.created_at),
    ];
    params.extend(guard_params);

    SqlStatement {
        sql: format!(
            "INSERT INTO events \
             (id, namespace, verb, substrate, actor, kind, outcome, payload, \
              payload_schema_version, profile_state_version, duration_us, \
              target_id, session_id, aggregate_kind, aggregate_id, created_at) \
             SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16 \
             WHERE ({remapped_guard})"
        ),
        params,
        label: Some("projection_worker.conditional_event_insert".into()),
    }
}

/// Remap `?1`, `?2`, ... in `guard_sql` to `?{offset}`, `?{offset+1}`, ...
pub(crate) fn remap_guard_params(guard_sql: &str, offset: usize) -> String {
    let max_idx: usize = (1..=32)
        .rev()
        .find(|&i| {
            let token = format!("?{i}");
            let mut pos = 0;
            while let Some(found) = guard_sql[pos..].find(&token) {
                let abs = pos + found;
                let after = abs + token.len();
                if guard_sql[after..]
                    .chars()
                    .next()
                    .is_none_or(|c| !c.is_ascii_digit())
                {
                    return true;
                }
                pos = abs + 1;
            }
            false
        })
        .unwrap_or(0);

    if max_idx == 0 {
        return guard_sql.to_string();
    }

    let mut result = guard_sql.to_string();
    for i in (1..=max_idx).rev() {
        let old = format!("?{i}");
        let new = format!("?{}", i + offset - 1);
        let mut out = String::with_capacity(result.len());
        let mut pos = 0;
        while let Some(found) = result[pos..].find(&old) {
            let abs = pos + found;
            let after = abs + old.len();
            if result[after..]
                .chars()
                .next()
                .is_none_or(|c| !c.is_ascii_digit())
            {
                out.push_str(&result[pos..abs]);
                out.push_str(&new);
            } else {
                out.push_str(&result[pos..after]);
            }
            pos = after;
        }
        out.push_str(&result[pos..]);
        result = out;
    }
    result
}
