use khive_storage::event::EventFilter;
use khive_storage::types::PageRequest;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{micros_to_iso, KhiveRuntime, NamespaceToken, RuntimeError};

use super::parsing::parse_handler_params;

impl KhiveRuntime {
    /// Read the brain debug event projection through the authorized token.
    pub async fn read_brain_events(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct EventsParams {
            limit: Option<u32>,
        }
        let p: EventsParams = parse_handler_params(params).map_err(RuntimeError::InvalidInput)?;

        let limit = p.limit.unwrap_or(20).min(100);
        let ns = token.namespace().as_str().to_string();

        let store = self.events(token)?;
        let filter = EventFilter {
            verbs: vec![
                "recall".into(),
                "search".into(),
                "brain.feedback".into(),
                "brain.emit".into(), // retained for backward-compat queries
                "get".into(),
                "remember".into(),
            ],
            ..EventFilter::default()
        };
        let _ = ns;
        let page = store
            .query_events(filter, PageRequest { offset: 0, limit })
            .await
            .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;

        let events: Vec<Value> = page
            .items
            .iter()
            .map(|e| {
                json!({
                    "id": e.id.to_string(),
                    "verb": e.verb,
                    "outcome": e.outcome,
                    "target_id": e.target_id.map(|t| t.to_string()),
                    "duration_us": e.duration_us,
                    "created_at": micros_to_iso(e.created_at),
                    "payload": e.payload,
                })
            })
            .collect();

        Ok(json!({
            "count": events.len(),
            "events": events,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Namespace;
    use khive_storage::Event;
    use khive_types::{EventKind, EventOutcome, SubstrateKind};
    use uuid::Uuid;

    fn event(id: u128, verb: &str, kind: EventKind) -> Event {
        let mut event = Event::new("local", verb, kind, SubstrateKind::Note, "fixture");
        event.id = Uuid::from_u128(id);
        event.created_at = id as i64;
        event
    }

    fn ids(result: &Value) -> Vec<String> {
        result["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["id"].as_str().unwrap().to_owned())
            .collect()
    }

    #[tokio::test]
    async fn recent_events_keep_closed_verbs_limits_namespace_and_all_kinds() {
        let runtime = KhiveRuntime::memory().unwrap();
        let token = runtime.authorize(Namespace::local()).unwrap();
        let other = runtime
            .authorize(Namespace::parse("other").unwrap())
            .unwrap();
        let store = runtime.events(&token).unwrap();
        let verbs = [
            "recall",
            "search",
            "brain.feedback",
            "brain.emit",
            "get",
            "remember",
        ];
        let mut rows: Vec<_> = (1..=101)
            .map(|id| {
                event(
                    id,
                    verbs[(id as usize - 1) % verbs.len()],
                    if id % 2 == 0 {
                        EventKind::Audit
                    } else {
                        EventKind::FeedbackExplicit
                    },
                )
            })
            .collect();
        rows.extend([
            event(201, "memory.recall", EventKind::FeedbackExplicit),
            event(202, "brain.state", EventKind::Audit),
            event(203, "create", EventKind::Audit),
            event(204, "other", EventKind::FeedbackExplicit),
        ]);
        store.append_events(rows).await.unwrap();
        let other_store = runtime.events(&other).unwrap();
        other_store
            .append_events(vec![
                event(301, "recall", EventKind::Audit),
                event(302, "brain.feedback", EventKind::FeedbackExplicit),
            ])
            .await
            .unwrap();
        let before = store
            .query_events(
                EventFilter::default(),
                PageRequest {
                    offset: 0,
                    limit: 200,
                },
            )
            .await
            .unwrap();
        let other_before = other_store
            .query_events(
                EventFilter::default(),
                PageRequest {
                    offset: 0,
                    limit: 200,
                },
            )
            .await
            .unwrap();
        assert_eq!(before.items.len(), 105);
        assert_eq!(other_before.items.len(), 2);

        let default = runtime.read_brain_events(&token, json!({})).await.unwrap();
        assert_eq!(
            default["count"], 20,
            "recent events retain the default20 window"
        );
        assert_eq!(
            ids(&default),
            (82..=101)
                .rev()
                .map(|id| Uuid::from_u128(id).to_string())
                .collect::<Vec<_>>(),
            "closed verbs, namespace and unfiltered event kinds select the exact newest rows"
        );
        let capped = runtime
            .read_brain_events(&token, json!({"limit": 1000}))
            .await
            .unwrap();
        assert_eq!(
            capped["count"], 100,
            "recent events cap the requested limit at100"
        );
        assert_eq!(
            ids(&capped),
            (2..=101)
                .rev()
                .map(|id| Uuid::from_u128(id).to_string())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            runtime
                .read_brain_events(&token, json!({"limit": null}))
                .await
                .unwrap(),
            default
        );
        assert_eq!(
            runtime
                .read_brain_events(&token, json!({"limit": 0}))
                .await
                .unwrap(),
            json!({"count": 0, "events": []})
        );
        let other_result = runtime.read_brain_events(&other, json!({})).await.unwrap();
        assert_eq!(other_result["count"], 2);
        assert_eq!(
            ids(&other_result),
            vec![
                Uuid::from_u128(302).to_string(),
                Uuid::from_u128(301).to_string()
            ]
        );
        assert_eq!(
            store
                .query_events(
                    EventFilter::default(),
                    PageRequest {
                        offset: 0,
                        limit: 200
                    }
                )
                .await
                .unwrap()
                .items,
            before.items,
            "recent reads preserve every stored local event"
        );
        assert_eq!(
            other_store
                .query_events(
                    EventFilter::default(),
                    PageRequest {
                        offset: 0,
                        limit: 200
                    }
                )
                .await
                .unwrap()
                .items,
            other_before.items,
            "recent reads preserve the other namespace too"
        );
    }

    #[tokio::test]
    async fn recent_events_preserve_exact_projection_and_null_target() {
        let runtime = KhiveRuntime::memory().unwrap();
        let token = runtime.authorize(Namespace::local()).unwrap();
        let mut first = event(1, "get", EventKind::Audit);
        first.created_at = 1_000_123;
        first.duration_us = -7;
        first.outcome = EventOutcome::Denied;
        first.target_id = Some(Uuid::from_u128(99));
        first.payload = json!({"nested": [null, {"value": "kept"}]});
        first.session_id = Some(Uuid::from_u128(100));
        let mut second = event(2, "brain.emit", EventKind::FeedbackExplicit);
        second.created_at = 2_000_456;
        second.duration_us = 42;
        second.outcome = EventOutcome::Error;
        second.payload = json!({"signal": "useful"});
        runtime
            .events(&token)
            .unwrap()
            .append_events(vec![first, second])
            .await
            .unwrap();
        let result = runtime.read_brain_events(&token, json!({})).await.unwrap();
        assert_eq!(
            result,
            json!({
                "count": 2,
                "events": [
                    {"id": Uuid::from_u128(2).to_string(), "verb": "brain.emit", "outcome": "error", "target_id": null, "duration_us": 42, "created_at": "1970-01-01T00:00:02.000456Z", "payload": {"signal": "useful"}},
                    {"id": Uuid::from_u128(1).to_string(), "verb": "get", "outcome": "denied", "target_id": Uuid::from_u128(99).to_string(), "duration_us": -7, "created_at": "1970-01-01T00:00:01.000123Z", "payload": {"nested": [null, {"value": "kept"}]}}
                ]
            }),
            "recent-event wire projection is exact, including omission of unprojected fields"
        );
    }

    #[tokio::test]
    async fn recent_events_keep_events_params_refusals() {
        let runtime = KhiveRuntime::memory().unwrap();
        let token = runtime.authorize(Namespace::local()).unwrap();
        for (params, expected) in [
            (
                json!({"namespace": "other"}),
                "unknown field `namespace`, expected `limit`",
            ),
            (
                json!({"kind": "audit"}),
                "unknown field `kind`, expected `limit`",
            ),
            (json!({"limit": -1}), "expected u32"),
            (json!({"limit": 4294967296_u64}), "expected u32"),
            (json!({"limit": "1"}), "expected u32"),
            (json!({"limit": 1.5}), "expected u32"),
            (json!(7), "expected struct EventsParams"),
        ] {
            let result = runtime.read_brain_events(&token, params.clone()).await;
            assert!(
                matches!(&result, Err(RuntimeError::InvalidInput(message)) if message.contains(expected)),
                "recent EventsParams refusal for {params}: {result:?}"
            );
        }
    }
}
