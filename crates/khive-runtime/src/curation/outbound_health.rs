use chrono::{DateTime, Utc};
use khive_storage::{SqlRow, SqlStatement, SqlValue};

use super::{KhiveRuntime, NamespaceToken, RuntimeError, RuntimeResult};

/// Uncapped metadata observation of one channel's eligible undelivered rows.
/// The delivery page is a separate read and keeps its own bounded row count.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutboundHealthSummary {
    pub observed_at: DateTime<Utc>,
    pub undelivered_rows: u64,
    pub backoff_rows: u64,
    pub oldest_undelivered_created_at: Option<DateTime<Utc>>,
    pub oldest_backoff_created_at: Option<DateTime<Utc>>,
    pub earliest_backoff_next_attempt_at: Option<DateTime<Utc>>,
    pub max_backoff_delivery_attempts: Option<u64>,
}

impl KhiveRuntime {
    /// Read backlog metadata on the runtime that owns comm's notes.
    /// Missing-slug rows belong here only when the caller has one adapter of this kind.
    pub async fn outbound_health_for_channel(
        &self,
        token: &NamespaceToken,
        to_prefix: &str,
        channel_slug: &str,
        include_legacy: bool,
    ) -> RuntimeResult<OutboundHealthSummary> {
        self.outbound_health_for_channel_at(
            token,
            to_prefix,
            channel_slug,
            include_legacy,
            Utc::now(),
        )
        .await
    }

    async fn outbound_health_for_channel_at(
        &self,
        token: &NamespaceToken,
        to_prefix: &str,
        channel_slug: &str,
        include_legacy: bool,
        observed_at: DateTime<Utc>,
    ) -> RuntimeResult<OutboundHealthSummary> {
        if !to_prefix
            .strip_suffix(':')
            .is_some_and(|head| !head.is_empty() && !head.contains(':'))
        {
            return Err(RuntimeError::InvalidInput(
                "outbox health requires a complete channel prefix ending in one colon".into(),
            ));
        }
        let _ = self.notes(token)?;
        // Match delivery's microsecond comparison, including the rest of this microsecond.
        let due_through = DateTime::from_timestamp_micros(observed_at.timestamp_micros())
            .and_then(|at| at.checked_add_signed(chrono::Duration::nanoseconds(999)))
            .ok_or_else(|| invalid_summary("observation timestamp"))?;
        let row = self
            .sql()
            .reader()
            .await?
            .query_row(SqlStatement {
                sql: crate::sql!("outbound_channel_health_summary").into(),
                params: vec![
                    SqlValue::Text(token.namespace().as_str().into()),
                    SqlValue::Text(to_prefix.into()),
                    SqlValue::Text(channel_slug.into()),
                    SqlValue::Integer(i64::from(include_legacy)),
                    SqlValue::Text(due_through.to_rfc3339()),
                ],
                label: Some("outbound-channel-health".into()),
            })
            .await?
            .ok_or_else(|| invalid_summary("aggregate row"))?;
        let max_backoff_delivery_attempts = match row.get("max_backoff_delivery_attempts") {
            Some(SqlValue::Null) => None,
            Some(SqlValue::Text(raw)) => Some(
                raw.parse::<u64>()
                    .map_err(|_| invalid_summary("max_backoff_delivery_attempts"))?,
            ),
            _ => return Err(invalid_summary("max_backoff_delivery_attempts")),
        };
        let earliest_backoff_next_attempt_at = match row.get("earliest_backoff_next_attempt_at") {
            Some(SqlValue::Null) => None,
            Some(SqlValue::Text(raw)) => Some(
                DateTime::parse_from_rfc3339(raw)
                    .map_err(|_| invalid_summary("earliest_backoff_next_attempt_at"))?
                    .with_timezone(&Utc),
            ),
            _ => return Err(invalid_summary("earliest_backoff_next_attempt_at")),
        };
        Ok(OutboundHealthSummary {
            observed_at,
            undelivered_rows: count(&row, "undelivered_rows")?,
            backoff_rows: count(&row, "backoff_rows")?,
            oldest_undelivered_created_at: timestamp(&row, "oldest_undelivered_created_at")?,
            oldest_backoff_created_at: timestamp(&row, "oldest_backoff_created_at")?,
            earliest_backoff_next_attempt_at,
            max_backoff_delivery_attempts,
        })
    }
}

fn invalid_summary(field: &str) -> RuntimeError {
    RuntimeError::Internal(format!("outbox health: invalid {field}"))
}

fn count(row: &SqlRow, field: &str) -> RuntimeResult<u64> {
    match row.get(field) {
        Some(SqlValue::Integer(value)) => u64::try_from(*value).map_err(|_| invalid_summary(field)),
        _ => Err(invalid_summary(field)),
    }
}

fn timestamp(row: &SqlRow, field: &str) -> RuntimeResult<Option<DateTime<Utc>>> {
    match row.get(field) {
        Some(SqlValue::Null) => Ok(None),
        Some(SqlValue::Integer(value)) => DateTime::from_timestamp_micros(*value)
            .map(Some)
            .ok_or_else(|| invalid_summary(field)),
        _ => Err(invalid_summary(field)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Namespace;
    use khive_storage::Note;
    use serde_json::{json, Value};

    fn at(raw: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(raw)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn fixture() -> (KhiveRuntime, NamespaceToken) {
        let runtime = KhiveRuntime::memory().unwrap();
        runtime.install_pack_owned_note_kinds(vec!["message".into()]);
        let token = runtime.authorize(Namespace::local()).unwrap();
        (runtime, token)
    }

    fn pending() -> Value {
        json!({
            "direction": "outbound", "to_actor": "email:recipient@example.com",
            "channel_slug": "sender@example.com", "next_attempt_at": "2035-01-01T00:00:00Z",
            "delivery_attempts": 9
        })
    }

    async fn seed(runtime: &KhiveRuntime, namespace: &str, properties: Value) -> Note {
        let mut note = Note::new(namespace, "message", "body is not needed by the summary");
        note.created_at = at("2020-01-01T00:00:00Z").timestamp_micros();
        note.properties = Some(properties);
        runtime
            .backend()
            .notes_for_namespace(namespace)
            .unwrap()
            .upsert_note(note.clone())
            .await
            .unwrap();
        note
    }

    async fn summary(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        legacy: bool,
    ) -> OutboundHealthSummary {
        runtime
            .outbound_health_for_channel_at(
                token,
                "email:",
                "sender@example.com",
                legacy,
                at("2030-01-01T00:00:00Z"),
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn backlog_preserves_channel_namespace_slug_and_delivery_eligibility() {
        let (runtime, token) = fixture();
        seed(&runtime, "local", pending()).await;
        let mut legacy = pending();
        legacy.as_object_mut().unwrap().remove("channel_slug");
        seed(&runtime, "local", legacy).await;
        let mut due = pending();
        due["next_attempt_at"] = json!("1990-01-01T00:00:00Z");
        let due_id = seed(&runtime, "local", due).await.id;
        let mut invalid_deadline = pending();
        let relaxed_only_deadline = "2035-01-01T00:00:00+0000";
        assert!(DateTime::parse_from_rfc3339(relaxed_only_deadline).is_err());
        assert!(relaxed_only_deadline.parse::<DateTime<Utc>>().is_ok());
        invalid_deadline["next_attempt_at"] = json!(relaxed_only_deadline);
        let invalid_deadline_id = seed(&runtime, "local", invalid_deadline).await.id;
        let mut nullable = pending();
        nullable["delivered_at"] = Value::Null;
        nullable["delivery_hold"] = Value::Null;
        seed(&runtime, "local", nullable).await;
        for (field, value) in [
            ("channel_slug", json!("another@example.com")),
            ("channel_slug", Value::Null),
            ("channel_slug", json!("")),
            ("direction", json!("inbound")),
            ("to_actor", json!("lambda:recipient")),
            ("to_actor", json!("emailx:recipient")),
            ("to_actor", json!("xemail:recipient")),
            ("delivery", json!("failed")),
            ("delivery", json!("delivered")),
            ("delivery_hold", json!(false)),
            ("delivered_at", json!("")),
        ] {
            let mut properties = pending();
            properties[field] = value;
            seed(&runtime, "local", properties).await;
        }
        seed(&runtime, "tenant-other", pending()).await;
        let mut deleted = seed(&runtime, "local", pending()).await;
        deleted.deleted_at = Some(deleted.created_at);
        runtime
            .backend()
            .notes_for_namespace("local")
            .unwrap()
            .upsert_note(deleted)
            .await
            .unwrap();
        let mut other_kind = seed(&runtime, "local", pending()).await;
        other_kind.kind = "observation".into();
        runtime
            .backend()
            .notes_for_namespace("local")
            .unwrap()
            .upsert_note(other_kind)
            .await
            .unwrap();

        let exact = summary(&runtime, &token, false).await;
        assert_eq!((exact.undelivered_rows, exact.backoff_rows), (4, 2));
        let with_legacy = summary(&runtime, &token, true).await;
        assert_eq!(
            (with_legacy.undelivered_rows, with_legacy.backoff_rows),
            (5, 3)
        );
        assert_eq!(with_legacy.max_backoff_delivery_attempts, Some(9));
        // All accepted due examples are also due at the real delivery query's current clock.
        let due = runtime
            .list_undelivered_outbound_messages_for_channel(
                &token,
                "email:",
                "sender@example.com",
                true,
                200,
            )
            .await
            .unwrap();
        assert_eq!(due.len(), 2);
        assert_eq!(
            due.iter()
                .map(|note| note.id)
                .collect::<std::collections::HashSet<_>>(),
            std::collections::HashSet::from([due_id, invalid_deadline_id]),
            "delivery must include the overdue and malformed-deadline rows only"
        );
        assert!(runtime
            .outbound_health_for_channel(&token, "email", "sender@example.com", true)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn backlog_uses_strict_instant_order_and_delivery_microsecond_boundary() {
        let (runtime, token) = fixture();
        let observed = at("2030-01-01T00:00:00.123456100Z");
        for deadline in [
            "2030-01-01T00:00:00.123456999Z",
            "2030-01-01T00:00:00.123457000Z",
            // Lexically earlier than the preceding value but two hours later as an instant.
            "2029-12-31T23:00:00-03:00",
        ] {
            let mut properties = pending();
            properties["next_attempt_at"] = json!(deadline);
            seed(&runtime, "local", properties).await;
        }
        let mut stale_key = seed(&runtime, "local", pending()).await;
        stale_key.properties.as_mut().unwrap()["next_attempt_at"] =
            json!("2030-01-01T00:00:00.123456999Z");
        runtime
            .sql()
            .writer()
            .await
            .unwrap()
            .execute(SqlStatement {
                sql: "UPDATE notes SET properties=?1 WHERE id=?2".into(),
                params: vec![
                    SqlValue::Text(stale_key.properties.unwrap().to_string()),
                    SqlValue::Text(stale_key.id.to_string()),
                ],
                label: None,
            })
            .await
            .unwrap();
        let summary = runtime
            .outbound_health_for_channel_at(&token, "email:", "sender@example.com", false, observed)
            .await
            .unwrap();
        assert_eq!((summary.undelivered_rows, summary.backoff_rows), (4, 2));
        assert_eq!(
            summary.earliest_backoff_next_attempt_at,
            Some(at("2030-01-01T00:00:00.123457Z"))
        );
        assert_eq!(summary.observed_at, observed);
    }

    #[tokio::test]
    async fn backlog_attempts_preserve_u64_values_and_ignore_other_json_types() {
        let (runtime, token) = fixture();
        let mut note = seed(&runtime, "local", pending()).await;
        for (value, expected) in [
            (json!(9), 9),
            (json!(10), 10),
            (json!(i64::MAX as u64 + 1), i64::MAX as u64 + 1),
            (json!(u64::MAX), u64::MAX),
            (json!(-1), 0),
            (json!(1.0), 0),
            (json!("42"), 0),
            (Value::Null, 0),
            (json!(true), 0),
            (json!([]), 0),
        ] {
            note.properties.as_mut().unwrap()["delivery_attempts"] = value.clone();
            runtime
                .backend()
                .notes_for_namespace("local")
                .unwrap()
                .upsert_note(note.clone())
                .await
                .unwrap();
            assert_eq!(
                summary(&runtime, &token, false)
                    .await
                    .max_backoff_delivery_attempts,
                Some(expected),
                "{value}"
            );
        }
        let mut nine = pending();
        nine["delivery_attempts"] = json!(9);
        seed(&runtime, "local", nine).await;
        let mut ten = pending();
        ten["delivery_attempts"] = json!(10);
        seed(&runtime, "local", ten).await;
        assert_eq!(
            summary(&runtime, &token, false)
                .await
                .max_backoff_delivery_attempts,
            Some(10)
        );
        note.properties.as_mut().unwrap()["delivery_attempts"] = json!(u64::MAX);
        runtime
            .backend()
            .notes_for_namespace("local")
            .unwrap()
            .upsert_note(note)
            .await
            .unwrap();
        assert_eq!(
            summary(&runtime, &token, false)
                .await
                .max_backoff_delivery_attempts,
            Some(u64::MAX)
        );
    }

    #[tokio::test]
    async fn backlog_is_not_capped_by_either_delivery_page_limit() {
        let (runtime, token) = fixture();
        let template = seed(&runtime, "local", pending()).await;
        runtime.sql().writer().await.unwrap().execute(SqlStatement {
            sql: "WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<10001)
                  INSERT INTO notes(id,namespace,kind,status,content,properties,created_at,updated_at,version)
                  SELECT printf('00000000-0000-4000-8000-%012d',i), namespace,kind,status,content,properties,created_at,updated_at,version
                  FROM n CROSS JOIN notes WHERE id=?1".into(),
            params: vec![SqlValue::Text(template.id.to_string())], label: None,
        }).await.unwrap();
        let backlog = summary(&runtime, &token, false).await;
        assert_eq!(
            (backlog.undelivered_rows, backlog.backoff_rows),
            (10002, 10002)
        );
        assert!(runtime
            .list_undelivered_outbound_messages_for_channel(
                &token,
                "email:",
                "sender@example.com",
                false,
                200
            )
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            backlog.oldest_backoff_created_at,
            Some(at("2020-01-01T00:00:00Z"))
        );
    }

    #[tokio::test]
    async fn backlog_reads_the_comm_backend_instead_of_its_core_backend() {
        let core_backend = std::sync::Arc::new(crate::StorageBackend::memory().unwrap());
        core_backend.prepare_core_schema().unwrap();
        let core = KhiveRuntime::from_backend(
            core_backend.clone(),
            crate::RuntimeConfig {
                db_path: None,
                ..crate::RuntimeConfig::no_embeddings()
            },
        );
        let comm_backend = std::sync::Arc::new(crate::StorageBackend::memory().unwrap());
        comm_backend.prepare_core_schema().unwrap();
        let runtime = KhiveRuntime::from_backend(
            comm_backend,
            crate::RuntimeConfig {
                db_path: None,
                backend_id: crate::BackendId::parse("comm").unwrap(),
                ..crate::RuntimeConfig::no_embeddings()
            },
        )
        .with_core_backend(core_backend);
        runtime.install_pack_owned_note_kinds(vec!["message".into()]);
        let token = runtime.authorize(Namespace::local()).unwrap();
        seed(&runtime, "local", pending()).await;
        for _ in 0..3 {
            let mut properties = pending();
            properties["delivery_attempts"] = json!(500);
            seed(&core, "local", properties).await;
        }
        let observed = summary(&runtime, &token, false).await;
        assert_eq!((observed.undelivered_rows, observed.backoff_rows), (1, 1));
        assert_eq!(observed.max_backoff_delivery_attempts, Some(9));
    }
}
