use khive_runtime::{KhiveRuntime, RuntimeError};
use khive_storage::types::{SqlRow, SqlStatement, SqlValue};
use serde_json::{json, Value};

pub const RECEIPTS_TABLE_SQL: &str = include_str!("../sql/git_receipts.sql");
pub const RECEIPTS_ACTOR_INDEX_SQL: &str = include_str!("../sql/git_receipts_actor_index.sql");
pub const RECEIPTS_SESSION_INDEX_SQL: &str = include_str!("../sql/git_receipts_session_index.sql");

const COLUMNS: &str = "id, namespace, actor, session_id, verb, repo, inputs, gate, policy, \
    fork_policy, credential, started_at, finished_at, disposition, result, reason";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Disposition {
    Unknown,
    Committed,
    NotCommitted,
}

impl Disposition {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Committed => "committed",
            Self::NotCommitted => "not_committed",
        }
    }

    fn parse(value: &str) -> Result<Self, RuntimeError> {
        match value {
            "unknown" => Ok(Self::Unknown),
            "committed" => Ok(Self::Committed),
            "not_committed" => Ok(Self::NotCommitted),
            _ => Err(invalid_row("disposition")),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Receipt {
    pub id: String,
    pub namespace: String,
    pub actor: String,
    pub session_id: Option<String>,
    pub verb: String,
    pub repo: String,
    pub inputs: Value,
    pub gate: Value,
    pub policy: Value,
    pub fork_policy: Value,
    pub credential: Value,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub disposition: Disposition,
    pub result: Value,
    pub reason: Option<String>,
}

impl Receipt {
    pub fn new(
        namespace: &str,
        actor: &str,
        verb: &str,
        repo: &str,
        inputs: Value,
        gate: Value,
        policy: Value,
    ) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            namespace: namespace.to_string(),
            actor: actor.to_string(),
            session_id: None,
            verb: verb.to_string(),
            repo: repo.to_string(),
            inputs,
            gate,
            policy,
            fork_policy: Value::Null,
            credential: Value::Null,
            started_at: chrono::Utc::now().timestamp_micros(),
            finished_at: None,
            disposition: Disposition::Unknown,
            result: Value::Null,
            reason: None,
        }
    }

    pub fn to_value(&self) -> Value {
        json!({
            "id": self.id,
            "namespace": self.namespace,
            "actor": self.actor,
            "session_id": self.session_id,
            "verb": self.verb,
            "repo": self.repo,
            "inputs": self.inputs,
            "gate": self.gate,
            "policy": self.policy,
            "fork_policy": self.fork_policy,
            "credential": self.credential,
            "timing": {
                "started_at": khive_runtime::micros_to_iso(self.started_at),
                "finished_at": self.finished_at.map(khive_runtime::micros_to_iso),
            },
            "disposition": self.disposition.as_str(),
            "result": self.result,
            "reason": self.reason,
        })
    }

    fn validate(&self) -> Result<(), RuntimeError> {
        if uuid::Uuid::parse_str(&self.id).is_err() {
            return Err(invalid_row("id"));
        }
        if chrono::DateTime::from_timestamp_micros(self.started_at).is_none() {
            return Err(invalid_row("started_at"));
        }
        if self
            .finished_at
            .is_some_and(|value| chrono::DateTime::from_timestamp_micros(value).is_none())
        {
            return Err(invalid_row("finished_at"));
        }
        if self.disposition != Disposition::Unknown && self.finished_at.is_none() {
            return Err(invalid_row("finished_at"));
        }
        for (name, value) in [("inputs", &self.inputs), ("gate", &self.gate)] {
            if !value.is_object() {
                return Err(invalid_row(name));
            }
        }
        for (name, value) in [
            ("policy", &self.policy),
            ("fork_policy", &self.fork_policy),
            ("credential", &self.credential),
        ] {
            if !value.is_null() && !value.is_object() {
                return Err(invalid_row(name));
            }
        }
        Ok(())
    }
}

pub struct ReceiptPage {
    pub receipts: Vec<Receipt>,
    pub next_offset: Option<u64>,
}

impl ReceiptPage {
    pub fn to_value(&self) -> Value {
        json!({
            "receipts": self.receipts.iter().map(Receipt::to_value).collect::<Vec<_>>(),
            "next_offset": self.next_offset,
        })
    }
}

fn invalid_row(column: &str) -> RuntimeError {
    RuntimeError::Internal(format!("invalid git receipt column: {column}"))
}

fn required_text(row: &SqlRow, column: &str) -> Result<String, RuntimeError> {
    match row.get(column) {
        Some(SqlValue::Text(value)) => Ok(value.clone()),
        _ => Err(invalid_row(column)),
    }
}

fn optional_text(row: &SqlRow, column: &str) -> Result<Option<String>, RuntimeError> {
    match row.get(column) {
        Some(SqlValue::Text(value)) => Ok(Some(value.clone())),
        Some(SqlValue::Null) => Ok(None),
        _ => Err(invalid_row(column)),
    }
}

fn required_int(row: &SqlRow, column: &str) -> Result<i64, RuntimeError> {
    match row.get(column) {
        Some(SqlValue::Integer(value)) => Ok(*value),
        _ => Err(invalid_row(column)),
    }
}

fn optional_int(row: &SqlRow, column: &str) -> Result<Option<i64>, RuntimeError> {
    match row.get(column) {
        Some(SqlValue::Integer(value)) => Ok(Some(*value)),
        Some(SqlValue::Null) => Ok(None),
        _ => Err(invalid_row(column)),
    }
}

fn json_column(row: &SqlRow, column: &str, nullable: bool) -> Result<Value, RuntimeError> {
    match row.get(column) {
        Some(SqlValue::Text(value)) => serde_json::from_str(value).map_err(|_| invalid_row(column)),
        Some(SqlValue::Json(value)) => Ok(value.clone()),
        Some(SqlValue::Null) if nullable => Ok(Value::Null),
        _ => Err(invalid_row(column)),
    }
}

fn decode(row: &SqlRow) -> Result<Receipt, RuntimeError> {
    let receipt = Receipt {
        id: required_text(row, "id")?,
        namespace: required_text(row, "namespace")?,
        actor: required_text(row, "actor")?,
        session_id: optional_text(row, "session_id")?,
        verb: required_text(row, "verb")?,
        repo: required_text(row, "repo")?,
        inputs: json_column(row, "inputs", false)?,
        gate: json_column(row, "gate", false)?,
        policy: json_column(row, "policy", true)?,
        fork_policy: json_column(row, "fork_policy", true)?,
        credential: json_column(row, "credential", true)?,
        started_at: required_int(row, "started_at")?,
        finished_at: optional_int(row, "finished_at")?,
        disposition: Disposition::parse(&required_text(row, "disposition")?)?,
        result: json_column(row, "result", false)?,
        reason: optional_text(row, "reason")?,
    };
    receipt.validate()?;
    Ok(receipt)
}

fn opt_text(value: Option<&str>) -> SqlValue {
    value.map_or(SqlValue::Null, |value| SqlValue::Text(value.to_string()))
}

fn json_sql(value: &Value) -> SqlValue {
    if value.is_null() {
        SqlValue::Null
    } else {
        SqlValue::Text(value.to_string())
    }
}

pub async fn insert(rt: &KhiveRuntime, receipt: &Receipt) -> Result<(), RuntimeError> {
    receipt.validate()?;
    if receipt.disposition != Disposition::Unknown || receipt.finished_at.is_some() {
        return Err(RuntimeError::Internal(
            "git receipt must be inserted as unfinished unknown".into(),
        ));
    }
    let mut writer = rt.sql().writer().await?;
    let changed = writer
        .execute(SqlStatement {
            sql: format!(
                "INSERT INTO git_receipts ({COLUMNS}) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)"
            ),
            params: vec![
                SqlValue::Text(receipt.id.clone()),
                SqlValue::Text(receipt.namespace.clone()),
                SqlValue::Text(receipt.actor.clone()),
                opt_text(receipt.session_id.as_deref()),
                SqlValue::Text(receipt.verb.clone()),
                SqlValue::Text(receipt.repo.clone()),
                SqlValue::Text(receipt.inputs.to_string()),
                SqlValue::Text(receipt.gate.to_string()),
                json_sql(&receipt.policy),
                json_sql(&receipt.fork_policy),
                json_sql(&receipt.credential),
                SqlValue::Integer(receipt.started_at),
                SqlValue::Null,
                SqlValue::Text(Disposition::Unknown.as_str().into()),
                SqlValue::Text(receipt.result.to_string()),
                opt_text(receipt.reason.as_deref()),
            ],
            label: Some("git_receipts_insert".into()),
        })
        .await?;
    if changed != 1 {
        return Err(RuntimeError::Internal(
            "git receipt insert did not write exactly one row".into(),
        ));
    }
    Ok(())
}

pub async fn persist(rt: &KhiveRuntime, receipt: &Receipt) -> Result<(), RuntimeError> {
    receipt.validate()?;
    let mut writer = rt.sql().writer().await?;
    let changed = writer
        .execute(SqlStatement {
            sql: "UPDATE git_receipts SET gate = ?1, policy = ?2, fork_policy = ?3, \
                  credential = ?4, finished_at = ?5, disposition = ?6, result = ?7, reason = ?8 \
                  WHERE id = ?9 AND namespace = ?10 AND actor = ?11 AND disposition = 'unknown' \
                  AND session_id IS ?12 AND verb = ?13 AND repo = ?14 AND inputs = ?15 \
                  AND started_at = ?16"
                .into(),
            params: vec![
                SqlValue::Text(receipt.gate.to_string()),
                json_sql(&receipt.policy),
                json_sql(&receipt.fork_policy),
                json_sql(&receipt.credential),
                receipt
                    .finished_at
                    .map_or(SqlValue::Null, SqlValue::Integer),
                SqlValue::Text(receipt.disposition.as_str().to_string()),
                SqlValue::Text(receipt.result.to_string()),
                opt_text(receipt.reason.as_deref()),
                SqlValue::Text(receipt.id.clone()),
                SqlValue::Text(receipt.namespace.clone()),
                SqlValue::Text(receipt.actor.clone()),
                opt_text(receipt.session_id.as_deref()),
                SqlValue::Text(receipt.verb.clone()),
                SqlValue::Text(receipt.repo.clone()),
                SqlValue::Text(receipt.inputs.to_string()),
                SqlValue::Integer(receipt.started_at),
            ],
            label: Some("git_receipts_persist".into()),
        })
        .await?;
    if changed != 1 {
        return Err(RuntimeError::Internal(format!(
            "git receipt {} was not an unchanged owned unknown row",
            receipt.id
        )));
    }
    Ok(())
}

pub async fn load_owned(
    rt: &KhiveRuntime,
    namespace: &str,
    actor: &str,
    id: &str,
) -> Result<Receipt, RuntimeError> {
    let mut reader = rt.sql().reader().await?;
    let rows = reader
        .query_all(SqlStatement {
            sql: format!(
                "SELECT {COLUMNS} FROM git_receipts WHERE namespace = ?1 AND actor = ?2 AND id = ?3"
            ),
            params: vec![
                SqlValue::Text(namespace.to_string()),
                SqlValue::Text(actor.to_string()),
                SqlValue::Text(id.to_string()),
            ],
            label: Some("git_receipts_load_owned".into()),
        })
        .await?;
    match rows.as_slice() {
        [row] => decode(row),
        [] => Err(RuntimeError::NotFound("git receipt not found".into())),
        _ => Err(RuntimeError::Internal(
            "git receipt id matched multiple rows".into(),
        )),
    }
}

pub async fn list_owned(
    rt: &KhiveRuntime,
    namespace: &str,
    actor: &str,
    repo: Option<&str>,
    session_id: Option<&str>,
    limit: u32,
    offset: u64,
) -> Result<ReceiptPage, RuntimeError> {
    if limit == 0 {
        return Err(RuntimeError::InvalidInput(
            "git.receipts limit must be positive".into(),
        ));
    }
    let sql_offset = i64::try_from(offset).map_err(|_| {
        RuntimeError::InvalidInput("git.receipts offset exceeds the supported range".into())
    })?;
    let mut reader = rt.sql().reader().await?;
    let rows = reader
        .query_all(SqlStatement {
            // Appended receipts do not shift offsets, even with a regressing clock.
            sql: format!(
                "SELECT {COLUMNS} FROM git_receipts WHERE namespace = ?1 AND actor = ?2 \
                 AND (?3 IS NULL OR repo = ?3) AND (?4 IS NULL OR session_id = ?4) \
                 ORDER BY rowid ASC LIMIT ?5 OFFSET ?6"
            ),
            params: vec![
                SqlValue::Text(namespace.to_string()),
                SqlValue::Text(actor.to_string()),
                opt_text(repo),
                opt_text(session_id),
                SqlValue::Integer(i64::from(limit) + 1),
                SqlValue::Integer(sql_offset),
            ],
            label: Some("git_receipts_list_owned".into()),
        })
        .await?;
    let mut receipts = rows.iter().map(decode).collect::<Result<Vec<_>, _>>()?;
    let next_offset = if receipts.len() > limit as usize {
        receipts.pop();
        Some(offset.checked_add(u64::from(limit)).ok_or_else(|| {
            RuntimeError::InvalidInput(
                "git.receipts next offset exceeds the supported range".into(),
            )
        })?)
    } else {
        None
    };
    Ok(ReceiptPage {
        receipts,
        next_offset,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn runtime() -> KhiveRuntime {
        let rt = KhiveRuntime::memory().expect("memory runtime");
        let mut writer = rt.sql().writer().await.expect("writer");
        for sql in [
            RECEIPTS_TABLE_SQL,
            RECEIPTS_ACTOR_INDEX_SQL,
            RECEIPTS_SESSION_INDEX_SQL,
        ] {
            writer
                .execute(SqlStatement {
                    sql: sql.into(),
                    params: vec![],
                    label: Some("git_receipts_test_schema".into()),
                })
                .await
                .expect("receipt schema");
        }
        drop(writer);
        rt
    }

    fn receipt() -> Receipt {
        let mut receipt = Receipt::new(
            "local",
            "actor:a",
            "git.commit",
            "/repo",
            json!({"branch": "work", "expected_head": "1".repeat(40)}),
            json!({"decision": "allow", "source": "git_write.allowed", "id": 0}),
            json!({"decision": "allow", "source": "policy", "id": "policy-a"}),
        );
        receipt.session_id = Some("session-a".into());
        receipt
    }

    #[tokio::test]
    async fn unknown_candidate_is_durable_and_terminal_settlement_cannot_be_overwritten() {
        let rt = runtime().await;
        let mut pending = receipt();
        insert(&rt, &pending).await.expect("insert unknown");
        let original = load_owned(&rt, "local", "actor:a", &pending.id)
            .await
            .expect("load unknown");
        assert_eq!(original.disposition, Disposition::Unknown);
        assert_eq!(original.finished_at, None);
        assert_eq!(original.to_value(), pending.to_value());

        pending.result = json!({"sha": "2".repeat(40), "receipt_id": pending.id});
        persist(&rt, &pending)
            .await
            .expect("persist prospective sha");
        let mut settled = load_owned(&rt, "local", "actor:a", &pending.id)
            .await
            .expect("load candidate");
        assert_eq!(settled.result, pending.result);
        settled.disposition = Disposition::Committed;
        settled.finished_at = Some(chrono::Utc::now().timestamp_micros());
        persist(&rt, &settled).await.expect("settle committed");

        assert!(persist(&rt, &pending).await.is_err());
        assert!(persist(&rt, &settled).await.is_err());
        let durable = load_owned(&rt, "local", "actor:a", &pending.id)
            .await
            .expect("load settled");
        assert_eq!(durable.to_value(), settled.to_value());
        assert!(matches!(
            load_owned(&rt, "local", "actor:b", &pending.id).await,
            Err(RuntimeError::NotFound(_))
        ));
        assert!(matches!(
            load_owned(&rt, "other", "actor:a", &pending.id).await,
            Err(RuntimeError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn pages_use_insertion_order_and_exact_lookahead_with_owner_filters() {
        let rt = runtime().await;
        let mut first = receipt();
        first.started_at = 200;
        insert(&rt, &first).await.expect("first");
        for (namespace, actor, repo, session) in [
            ("other", "actor:a", "/repo", "session-a"),
            ("local", "actor:b", "/repo", "session-a"),
            ("local", "actor:a", "/other", "session-a"),
            ("local", "actor:a", "/repo", "session-b"),
        ] {
            let mut excluded = receipt();
            excluded.namespace = namespace.into();
            excluded.actor = actor.into();
            excluded.repo = repo.into();
            excluded.session_id = Some(session.into());
            insert(&rt, &excluded).await.expect("excluded receipt");
        }
        let mut second = receipt();
        second.started_at = 100;
        insert(&rt, &second).await.expect("second");
        let page = list_owned(
            &rt,
            "local",
            "actor:a",
            Some("/repo"),
            Some("session-a"),
            1,
            0,
        )
        .await
        .expect("first page");
        assert_eq!(page.receipts.len(), 1);
        assert_eq!(page.receipts[0].id, first.id);
        assert_eq!(page.next_offset, Some(1));

        let mut third = receipt();
        third.started_at = 50;
        insert(&rt, &third)
            .await
            .expect("appended after first page");
        let page = list_owned(
            &rt,
            "local",
            "actor:a",
            Some("/repo"),
            Some("session-a"),
            2,
            1,
        )
        .await
        .expect("last page");
        assert_eq!(page.receipts.len(), 2);
        assert_eq!(page.receipts[0].id, second.id);
        assert_eq!(page.receipts[1].id, third.id);
        assert_eq!(page.next_offset, None);
        let all = list_owned(&rt, "local", "actor:a", None, None, 10, 0)
            .await
            .expect("optional filters");
        assert_eq!(all.receipts.len(), 5);
        assert_eq!(all.next_offset, None);
    }

    #[tokio::test]
    async fn malformed_stored_row_fails_the_page_instead_of_disappearing() {
        let rt = runtime().await;
        let good = receipt();
        let bad = receipt();
        insert(&rt, &good).await.expect("good");
        insert(&rt, &bad).await.expect("bad fixture");
        let mut writer = rt.sql().writer().await.expect("writer");
        writer
            .execute(SqlStatement {
                sql: "UPDATE git_receipts SET policy = '[]' WHERE id = ?1".into(),
                params: vec![SqlValue::Text(bad.id.clone())],
                label: Some("git_receipts_test_corrupt".into()),
            })
            .await
            .expect("corrupt shape with valid JSON");
        drop(writer);
        assert!(list_owned(&rt, "local", "actor:a", None, None, 10, 0)
            .await
            .is_err());
        assert!(load_owned(&rt, "local", "actor:a", &bad.id).await.is_err());
        assert!(load_owned(&rt, "local", "actor:a", &good.id).await.is_ok());
    }

    #[tokio::test]
    async fn writes_reject_terminal_insert_and_changed_operation_identity() {
        let rt = runtime().await;
        let mut pending = receipt();
        let mut terminal = pending.clone();
        terminal.disposition = Disposition::NotCommitted;
        terminal.finished_at = Some(terminal.started_at);
        assert!(insert(&rt, &terminal).await.is_err());
        insert(&rt, &pending).await.expect("initial unknown");
        assert!(insert(&rt, &pending).await.is_err());
        pending.inputs["branch"] = json!("different");
        assert!(persist(&rt, &pending).await.is_err());
        let durable = load_owned(&rt, "local", "actor:a", &pending.id)
            .await
            .expect("original receipt");
        assert_eq!(durable.inputs["branch"], "work");
        assert_eq!(durable.disposition, Disposition::Unknown);
    }
}
