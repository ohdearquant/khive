//! Policy rows and grant rows: the two pack-owned tables, and the decision
//! function that turns them into allow, deny or ask for one actor and tool.

use chrono::Utc;
use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::{micros_to_iso, KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::types::{SqlRow, SqlStatement, SqlValue};

pub fn now_micros() -> i64 {
    Utc::now().timestamp_micros()
}

/// The caller's actor as one label, `kind:id`, except that the plain `actor`
/// kind collapses to its id so a configured `lambda:khive` reads back as
/// itself.
pub fn actor_label(token: &NamespaceToken) -> String {
    let actor = token.actor();
    if actor.kind == "actor" {
        actor.id.clone()
    } else {
        format!("{}:{}", actor.kind, actor.id)
    }
}

fn text(row: &SqlRow, col: &str) -> Option<String> {
    match row.get(col) {
        Some(SqlValue::Text(s)) => Some(s.clone()),
        Some(SqlValue::Uuid(u)) => Some(u.to_string()),
        Some(SqlValue::Json(v)) => Some(v.to_string()),
        _ => None,
    }
}

fn int(row: &SqlRow, col: &str) -> Option<i64> {
    match row.get(col) {
        Some(SqlValue::Integer(i)) => Some(*i),
        Some(SqlValue::Float(f)) => Some(*f as i64),
        _ => None,
    }
}

fn opt_text(v: Option<&str>) -> SqlValue {
    match v {
        Some(s) => SqlValue::Text(s.to_string()),
        None => SqlValue::Null,
    }
}

fn opt_int(v: Option<i64>) -> SqlValue {
    match v {
        Some(i) => SqlValue::Integer(i),
        None => SqlValue::Null,
    }
}

fn iso(v: Option<i64>) -> Value {
    match v {
        Some(us) => Value::String(micros_to_iso(us)),
        None => Value::Null,
    }
}

/// `*` matches everything, a trailing `*` matches a prefix, anything else is
/// an exact match.
pub(crate) fn pattern_matches(pattern: &str, value: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return value.starts_with(prefix);
    }
    pattern == value
}

fn specificity(pattern: &str) -> u8 {
    if pattern == "*" {
        0
    } else if pattern.ends_with('*') {
        1
    } else {
        2
    }
}

fn decision_rank(decision: &str) -> u8 {
    match decision {
        "deny" => 2,
        "ask" => 1,
        _ => 0,
    }
}

#[derive(Debug, Clone)]
pub(crate) struct PolicyRow {
    pub id: String,
    pub actor: String,
    pub tool: String,
    pub decision: String,
    pub note: Option<String>,
    pub created_at: i64,
    pub created_by: Option<String>,
}

impl PolicyRow {
    fn from_row(row: &SqlRow) -> Option<Self> {
        Some(Self {
            id: text(row, "id")?,
            actor: text(row, "actor")?,
            tool: text(row, "tool")?,
            decision: text(row, "decision")?,
            note: text(row, "note"),
            created_at: int(row, "created_at")?,
            created_by: text(row, "created_by"),
        })
    }

    pub(crate) fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "actor": self.actor,
            "tool": self.tool,
            "decision": self.decision,
            "note": self.note,
            "created_at": micros_to_iso(self.created_at),
            "created_by": self.created_by,
        })
    }
}

#[derive(Debug, Clone)]
pub(crate) struct GrantRow {
    pub id: String,
    pub actor: String,
    pub tool: String,
    pub scope: Option<String>,
    pub reason: Option<String>,
    pub status: String,
    pub requested_at: i64,
    pub decided_at: Option<i64>,
    pub decided_by: Option<String>,
    pub expires_at: Option<i64>,
    pub decision_note: Option<String>,
}

impl GrantRow {
    fn from_row(row: &SqlRow) -> Option<Self> {
        Some(Self {
            id: text(row, "id")?,
            actor: text(row, "actor")?,
            tool: text(row, "tool")?,
            scope: text(row, "scope"),
            reason: text(row, "reason"),
            status: text(row, "status")?,
            requested_at: int(row, "requested_at")?,
            decided_at: int(row, "decided_at"),
            decided_by: text(row, "decided_by"),
            expires_at: int(row, "expires_at"),
            decision_note: text(row, "decision_note"),
        })
    }

    pub(crate) fn is_active(&self, now: i64) -> bool {
        self.status == "granted" && self.expires_at.is_none_or(|e| e > now)
    }

    pub(crate) fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "actor": self.actor,
            "tool": self.tool,
            "scope": self.scope,
            "reason": self.reason,
            "status": self.status,
            "requested_at": micros_to_iso(self.requested_at),
            "decided_at": iso(self.decided_at),
            "decided_by": self.decided_by,
            "expires_at": iso(self.expires_at),
            "decision_note": self.decision_note,
        })
    }
}

const GRANT_COLUMNS: &str = "id, actor, tool, scope, reason, status, requested_at, decided_at, decided_by, expires_at, decision_note";
const POLICY_COLUMNS: &str = "id, actor, tool, decision, note, created_at, created_by";

pub(crate) async fn list_policies(
    rt: &KhiveRuntime,
    ns: &str,
    actor: Option<&str>,
    limit: u32,
) -> Result<Vec<PolicyRow>, RuntimeError> {
    let mut reader = rt.sql().reader().await?;
    let rows = reader
        .query_all(SqlStatement {
            sql: format!(
                "SELECT {POLICY_COLUMNS} FROM tool_policy WHERE namespace = ?1 \
                 ORDER BY created_at DESC LIMIT ?2"
            ),
            params: vec![
                SqlValue::Text(ns.to_string()),
                SqlValue::Integer(i64::from(limit)),
            ],
            label: Some("tool_policy_list".into()),
        })
        .await?;
    Ok(rows
        .iter()
        .filter_map(PolicyRow::from_row)
        .filter(|p| actor.is_none_or(|a| pattern_matches(&p.actor, a) || p.actor == a))
        .collect())
}

pub(crate) async fn insert_policy(
    rt: &KhiveRuntime,
    ns: &str,
    actor: &str,
    tool: &str,
    decision: &str,
    note: Option<&str>,
    created_by: &str,
) -> Result<PolicyRow, RuntimeError> {
    let row = PolicyRow {
        id: Uuid::new_v4().to_string(),
        actor: actor.to_string(),
        tool: tool.to_string(),
        decision: decision.to_string(),
        note: note.map(str::to_string),
        created_at: now_micros(),
        created_by: Some(created_by.to_string()),
    };
    let mut writer = rt.sql().writer().await?;
    writer
        .execute(SqlStatement {
            sql: format!(
                "INSERT INTO tool_policy ({POLICY_COLUMNS}, namespace) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"
            ),
            params: vec![
                SqlValue::Text(row.id.clone()),
                SqlValue::Text(row.actor.clone()),
                SqlValue::Text(row.tool.clone()),
                SqlValue::Text(row.decision.clone()),
                opt_text(row.note.as_deref()),
                SqlValue::Integer(row.created_at),
                opt_text(row.created_by.as_deref()),
                SqlValue::Text(ns.to_string()),
            ],
            label: Some("tool_policy_insert".into()),
        })
        .await?;
    Ok(row)
}

pub(crate) async fn list_grants(
    rt: &KhiveRuntime,
    ns: &str,
    status: Option<&str>,
    actor: Option<&str>,
    tool: Option<&str>,
    limit: u32,
) -> Result<Vec<GrantRow>, RuntimeError> {
    let mut sql = format!("SELECT {GRANT_COLUMNS} FROM tool_grants WHERE namespace = ?1");
    let mut params = vec![SqlValue::Text(ns.to_string())];
    if let Some(status) = status {
        params.push(SqlValue::Text(status.to_string()));
        sql.push_str(&format!(" AND status = ?{}", params.len()));
    }
    if let Some(tool) = tool {
        params.push(SqlValue::Text(tool.to_string()));
        sql.push_str(&format!(" AND tool = ?{}", params.len()));
    }
    params.push(SqlValue::Integer(i64::from(limit)));
    sql.push_str(&format!(
        " ORDER BY requested_at DESC LIMIT ?{}",
        params.len()
    ));
    let mut reader = rt.sql().reader().await?;
    let rows = reader
        .query_all(SqlStatement {
            sql,
            params,
            label: Some("tool_grants_list".into()),
        })
        .await?;
    Ok(rows
        .iter()
        .filter_map(GrantRow::from_row)
        .filter(|g| actor.is_none_or(|a| g.actor == a))
        .collect())
}

pub(crate) async fn get_grant(
    rt: &KhiveRuntime,
    ns: &str,
    id: &str,
) -> Result<GrantRow, RuntimeError> {
    let mut reader = rt.sql().reader().await?;
    let rows = reader
        .query_all(SqlStatement {
            sql: format!(
                "SELECT {GRANT_COLUMNS} FROM tool_grants WHERE namespace = ?1 \
                 AND (id = ?2 OR id LIKE ?3) LIMIT 3"
            ),
            params: vec![
                SqlValue::Text(ns.to_string()),
                SqlValue::Text(id.to_string()),
                SqlValue::Text(format!("{id}%")),
            ],
            label: Some("tool_grants_get".into()),
        })
        .await?;
    let mut found: Vec<GrantRow> = rows.iter().filter_map(GrantRow::from_row).collect();
    match found.len() {
        0 => Err(RuntimeError::NotFound(format!(
            "tool grant {id:?} not found"
        ))),
        1 => Ok(found.remove(0)),
        _ => Err(RuntimeError::Ambiguous(format!(
            "tool grant prefix {id:?} matches more than one row"
        ))),
    }
}

pub(crate) async fn insert_grant_request(
    rt: &KhiveRuntime,
    ns: &str,
    actor: &str,
    tool: &str,
    scope: Option<&str>,
    reason: Option<&str>,
) -> Result<GrantRow, RuntimeError> {
    let row = GrantRow {
        id: Uuid::new_v4().to_string(),
        actor: actor.to_string(),
        tool: tool.to_string(),
        scope: scope.map(str::to_string),
        reason: reason.map(str::to_string),
        status: "requested".to_string(),
        requested_at: now_micros(),
        decided_at: None,
        decided_by: None,
        expires_at: None,
        decision_note: None,
    };
    let mut writer = rt.sql().writer().await?;
    writer
        .execute(SqlStatement {
            sql: "INSERT INTO tool_grants (id, namespace, actor, tool, scope, reason, status, requested_at) \
                  VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"
                .into(),
            params: vec![
                SqlValue::Text(row.id.clone()),
                SqlValue::Text(ns.to_string()),
                SqlValue::Text(row.actor.clone()),
                SqlValue::Text(row.tool.clone()),
                opt_text(row.scope.as_deref()),
                opt_text(row.reason.as_deref()),
                SqlValue::Text(row.status.clone()),
                SqlValue::Integer(row.requested_at),
            ],
            label: Some("tool_grants_insert".into()),
        })
        .await?;
    Ok(row)
}

pub(crate) async fn set_grant_status(
    rt: &KhiveRuntime,
    ns: &str,
    id: &str,
    status: &str,
    decided_by: &str,
    expires_at: Option<i64>,
    note: Option<&str>,
) -> Result<GrantRow, RuntimeError> {
    let now = now_micros();
    let mut writer = rt.sql().writer().await?;
    let affected = writer
        .execute(SqlStatement {
            sql: "UPDATE tool_grants SET status = ?1, decided_at = ?2, decided_by = ?3, \
                  expires_at = ?4, decision_note = ?5 WHERE namespace = ?6 AND id = ?7"
                .into(),
            params: vec![
                SqlValue::Text(status.to_string()),
                SqlValue::Integer(now),
                SqlValue::Text(decided_by.to_string()),
                opt_int(expires_at),
                opt_text(note),
                SqlValue::Text(ns.to_string()),
                SqlValue::Text(id.to_string()),
            ],
            label: Some("tool_grants_decide".into()),
        })
        .await?;
    drop(writer);
    if affected == 0 {
        return Err(RuntimeError::NotFound(format!(
            "tool grant {id:?} not found"
        )));
    }
    get_grant(rt, ns, id).await
}

/// One policy decision and where it came from.
#[derive(Debug, Clone)]
pub struct Decision {
    pub decision: String,
    pub source: String,
    pub grant_id: Option<String>,
    pub policy_id: Option<String>,
    pub expires_at: Option<i64>,
    pub side_effect: Option<String>,
}

impl Decision {
    pub fn to_json(&self) -> Value {
        json!({
            "decision": self.decision,
            "source": self.source,
            "grant_id": self.grant_id,
            "policy_id": self.policy_id,
            "expires_at": iso(self.expires_at),
            "side_effect": self.side_effect,
        })
    }
}

/// Resolution order: an active grant allows; otherwise the most specific
/// matching policy row decides (ties: deny over ask over allow); otherwise
/// the side-effect default (read allows, everything else asks).
pub async fn decide(
    rt: &KhiveRuntime,
    ns: &str,
    actor: &str,
    tool: &str,
    side_effect: Option<&str>,
) -> Result<Decision, RuntimeError> {
    let now = now_micros();
    let grants = list_grants(rt, ns, Some("granted"), None, None, 500).await?;
    if let Some(g) = grants.iter().find(|g| {
        g.is_active(now) && pattern_matches(&g.actor, actor) && pattern_matches(&g.tool, tool)
    }) {
        return Ok(Decision {
            decision: "allow".into(),
            source: "grant".into(),
            grant_id: Some(g.id.clone()),
            policy_id: None,
            expires_at: g.expires_at,
            side_effect: side_effect.map(str::to_string),
        });
    }
    let policies = list_policies(rt, ns, None, 1000).await?;
    let best = policies
        .iter()
        .filter(|p| pattern_matches(&p.actor, actor) && pattern_matches(&p.tool, tool))
        .max_by_key(|p| {
            (
                specificity(&p.actor) + specificity(&p.tool),
                decision_rank(&p.decision),
            )
        });
    if let Some(p) = best {
        return Ok(Decision {
            decision: p.decision.clone(),
            source: "policy".into(),
            grant_id: None,
            policy_id: Some(p.id.clone()),
            expires_at: None,
            side_effect: side_effect.map(str::to_string),
        });
    }
    let decision = if side_effect == Some("read") {
        "allow"
    } else {
        "ask"
    };
    Ok(Decision {
        decision: decision.into(),
        source: "default".into(),
        grant_id: None,
        policy_id: None,
        expires_at: None,
        side_effect: side_effect.map(str::to_string),
    })
}

#[cfg(test)]
mod tests {
    use super::pattern_matches;

    #[test]
    fn patterns() {
        assert!(pattern_matches("*", "anything"));
        assert!(pattern_matches("mcp.*", "mcp.fetch"));
        assert!(!pattern_matches("mcp.*", "comm.send"));
        assert!(pattern_matches("comm.send", "comm.send"));
        assert!(!pattern_matches("comm.send", "comm.sendx"));
    }
}
