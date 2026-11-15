use std::collections::HashMap;

use khive_pack_comm::CommPack;
use khive_pack_kg::KgPack;
use khive_runtime::{
    BackendId, KhiveRuntime, Namespace, PackRegistry, RequestIdentity, RuntimeConfig, RuntimeError,
    VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::note::{FilterOp, NoteFilter, NoteKeyAssignOutcome, PropertyFilter};
use khive_storage::{Note, PageRequest, SqlValue};
use serde_json::{json, Value};
use uuid::Uuid;

const PREFIX: &str = "actor-filter/";

struct Stored {
    note: Note,
    sequence: i64,
}

struct Fixture {
    main: KhiveRuntime,
    comm: KhiveRuntime,
    registry: VerbRegistry,
    rows: Vec<Stored>,
}

impl Fixture {
    fn new(split: bool) -> Self {
        let runtime = |id: &str| {
            KhiveRuntime::new(RuntimeConfig {
                backend_id: BackendId::parse(id).unwrap(),
                db_path: None,
                ..RuntimeConfig::no_embeddings()
            })
            .unwrap()
        };
        let main = runtime("main");
        let comm = if split { runtime("comm") } else { main.clone() };
        let _ = (KgPack::new(main.clone()), CommPack::new(comm.clone()));
        let mut builder = VerbRegistryBuilder::new();
        PackRegistry::register_packs_with_runtimes(
            &["kg".into(), "comm".into()],
            &HashMap::from([("kg".into(), main.clone()), ("comm".into(), comm.clone())]),
            &main,
            &mut builder,
        )
        .unwrap();
        Self {
            main,
            comm,
            registry: builder.build().unwrap(),
            rows: Vec::new(),
        }
    }

    async fn call(
        &self,
        actor: Option<&str>,
        verb: &str,
        args: Value,
    ) -> Result<Value, RuntimeError> {
        self.registry
            .dispatch_with_identity(
                verb,
                args,
                Some(RequestIdentity {
                    namespace: "local".into(),
                    actor_id: actor.map(str::to_owned),
                    ..Default::default()
                }),
            )
            .await
    }

    async fn track(&mut self, id: Uuid) -> Uuid {
        let token = self.comm.authorize(Namespace::local()).unwrap();
        let store = self.comm.notes(&token).unwrap();
        let mut note = store.get_note(id).await.unwrap().unwrap();
        let original = note.clone();
        note.updated_at = 1_700_000_000_000_000 - self.rows.len() as i64;
        store.upsert_note(note).await.unwrap();
        let key = format!("{PREFIX}{:04}", self.rows.len());
        assert_eq!(
            store
                .assign_note_key_if_unset(id, &key, "local")
                .await
                .unwrap(),
            NoteKeyAssignOutcome::Assigned
        );
        let stored = store.get_note(id).await.unwrap().unwrap();
        assert_eq!(stored.key.as_deref(), Some(key.as_str()));
        assert_eq!(stored.properties, original.properties);
        assert_eq!(stored.content, original.content);
        assert_eq!(stored.created_at, original.created_at);
        assert!(stored.version > original.version);
        let sequence = store.note_sequence(id).await.unwrap().unwrap();
        self.rows.push(Stored {
            note: stored,
            sequence,
        });
        id
    }

    async fn seed(&mut self, note: Note) -> Uuid {
        let id = note.id;
        let token = self.comm.authorize(Namespace::local()).unwrap();
        self.comm
            .notes(&token)
            .unwrap()
            .upsert_note(note)
            .await
            .unwrap();
        self.track(id).await
    }

    async fn send(&mut self, from: &str, to: &str) -> (Uuid, Uuid) {
        let sent = self
            .call(
                Some(from),
                "comm.send",
                json!({"to": to, "subject": "actor filter", "content": "message filter fixture"}),
            )
            .await
            .unwrap();
        let out = Uuid::parse_str(sent["full_id"].as_str().unwrap()).unwrap();
        let token = self.comm.authorize(Namespace::local()).unwrap();
        let note = self
            .comm
            .notes(&token)
            .unwrap()
            .get_note(out)
            .await
            .unwrap()
            .unwrap();
        let props = note.properties.as_ref().unwrap();
        assert_eq!(props["from"], "local");
        assert_eq!(props["to"], "local");
        assert_eq!(props["from_actor"], from);
        assert_eq!(props["to_actor"], to);
        assert!(note.key.is_none());
        let siblings = self
            .comm
            .notes(&token)
            .unwrap()
            .query_notes_filtered(
                "local",
                &NoteFilter {
                    kind: Some("message".into()),
                    property_filters: vec![PropertyFilter {
                        json_path: "$.outbound_ref".into(),
                        op: FilterOp::Eq,
                        value: SqlValue::Text(out.to_string()),
                    }],
                    ..Default::default()
                },
                PageRequest {
                    limit: 2,
                    offset: 0,
                },
            )
            .await
            .unwrap();
        assert_eq!(siblings.items.len(), 1, "one actual inbound sibling");
        let sibling = &siblings.items[0];
        assert!(sibling.key.is_none());
        let routing = sibling.properties.as_ref().unwrap();
        assert_eq!(routing["direction"], "inbound");
        assert_eq!(routing["from_actor"], from);
        assert_eq!(routing["to_actor"], to);
        assert_eq!(routing["outbound_ref"], out.to_string());
        let incoming = sibling.id;
        self.track(out).await;
        self.track(incoming).await;
        (out, incoming)
    }

    fn row(&self, id: Uuid) -> &Stored {
        self.rows.iter().find(|row| row.note.id == id).unwrap()
    }

    fn ordered(&self, ids: &[Uuid], route: &str) -> Vec<Uuid> {
        let mut rows: Vec<_> = ids.iter().map(|id| self.row(*id)).collect();
        rows.sort_by(|a, b| match route {
            "cursor" => a.sequence.cmp(&b.sequence),
            "keyed" => b
                .note
                .updated_at
                .cmp(&a.note.updated_at)
                .then_with(|| b.note.key.cmp(&a.note.key))
                .then_with(|| a.note.id.cmp(&b.note.id)),
            "offset" => b
                .note
                .created_at
                .cmp(&a.note.created_at)
                .then_with(|| a.note.id.cmp(&b.note.id)),
            _ => unreachable!(),
        });
        rows.iter().map(|row| row.note.id).collect()
    }

    async fn assert_routes(&self, actor: Option<&str>, args: Value, expected: &[Uuid]) {
        for keyed in [false, true] {
            let order = self.ordered(expected, if keyed { "keyed" } else { "offset" });
            for offset in 0..=order.len() {
                let mut request = args.clone();
                request["offset"] = json!(offset);
                request["limit"] = json!(1);
                if keyed {
                    request["key_prefix"] = json!(PREFIX);
                }
                let page = self.call(actor, "list", request.clone()).await.unwrap();
                let want: Vec<_> = order.iter().skip(offset).take(1).copied().collect();
                assert_eq!(ids(&page, "items"), want, "exact offset IDs: {request}");
                assert_eq!(page["has_more"], json!(offset + 1 < order.len()));
                assert_eq!(page["effective_limit"], 1);
                assert!(page.get("scan_incomplete").is_none());
            }
            let order = self.ordered(expected, if keyed { "keyed" } else { "cursor" });
            let mut request = args.clone();
            request["after"] = json!("");
            request["limit"] = json!(1);
            if keyed {
                request["key_prefix"] = json!(PREFIX);
            }
            self.assert_walk(actor, request, &order, keyed).await;
            if keyed && !order.is_empty() {
                let mut request = args.clone();
                request["key_prefix"] = json!(PREFIX);
                request["after_key"] = json!(self.row(order[0]).note.key);
                request["limit"] = json!(1);
                self.assert_walk(actor, request, &order[1..], true).await;
            }
        }
    }

    async fn assert_walk(
        &self,
        actor: Option<&str>,
        mut request: Value,
        expected: &[Uuid],
        keyed: bool,
    ) {
        let mut seen = Vec::new();
        for index in 0..=expected.len() {
            let page = self.call(actor, "list", request.clone()).await.unwrap();
            let want: Vec<_> = expected.iter().skip(index).take(1).copied().collect();
            let got = ids(&page, "notes");
            assert_eq!(got, want, "exact cursor IDs: {request}");
            seen.extend(got);
            assert_eq!(page["has_more"], json!(index + 1 < expected.len()));
            assert!(page.get("scan_incomplete").is_none());
            let Some(cursor) = page["next_after"].as_str() else {
                assert_eq!(seen, expected, "complete walk");
                return;
            };
            assert!(index + 1 < expected.len(), "unexpected continuation");
            if keyed {
                let decoded: Value =
                    serde_json::from_str(cursor.strip_prefix("nk1:").unwrap()).unwrap();
                assert_eq!(decoded["id"], expected[index].to_string());
                assert_eq!(decoded["key"], json!(self.row(expected[index]).note.key));
            } else {
                assert_eq!(cursor, expected[index].to_string());
            }
            request.as_object_mut().unwrap().remove("after_key");
            request["after"] = json!(cursor);
        }
        panic!("cursor walk did not terminate");
    }

    async fn finish(self) {
        let token = self.comm.authorize(Namespace::local()).unwrap();
        let store = self.comm.notes(&token).unwrap();
        for row in &self.rows {
            let fresh = store.get_note(row.note.id).await.unwrap().unwrap();
            assert_eq!(fresh.properties, row.note.properties);
            assert_eq!(
                serde_json::to_value(fresh).unwrap(),
                serde_json::to_value(&row.note).unwrap()
            );
            assert_eq!(
                store.note_sequence(row.note.id).await.unwrap(),
                Some(row.sequence)
            );
        }
        self.registry.shutdown_audit_batch().await.unwrap();
    }
}

fn ids(page: &Value, field: &str) -> Vec<Uuid> {
    page[field]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| Uuid::parse_str(row["id"].as_str().unwrap()).unwrap())
        .collect()
}

#[tokio::test]
async fn list_message_from_prefers_actor_across_pagination_routes() {
    for split in [false, true] {
        let mut f = Fixture::new(split);
        let (_, first) = f.send("alice", "bob").await;
        f.send("dave", "bob").await;
        let (_, second) = f.send("alice", "bob").await;
        f.send("alice", "carol").await;
        for mut args in [
            json!({"kind": "message"}),
            json!({"kind": "note", "note_kind": "message"}),
        ] {
            args["from"] = json!("alice");
            f.assert_routes(Some("bob"), args.clone(), &[first, second])
                .await;
            for value in ["local", "Alice", "nobody"] {
                args["from"] = json!(value);
                f.assert_routes(Some("bob"), args.clone(), &[]).await;
            }
        }
        if !split {
            f.assert_routes(
                Some("bob"),
                json!({"kind": "note", "from": "alice"}),
                &[first, second],
            )
            .await;
        }
        f.finish().await;
    }
}

#[tokio::test]
async fn list_message_to_prefers_actor_across_pagination_routes() {
    for split in [false, true] {
        let mut f = Fixture::new(split);
        let (first, _) = f.send("alice", "bob").await;
        f.send("alice", "carol").await;
        let (second, _) = f.send("alice", "bob").await;
        f.send("dave", "bob").await;
        for mut args in [
            json!({"kind": "message"}),
            json!({"kind": "note", "note_kind": "message"}),
        ] {
            args["to"] = json!("bob");
            f.assert_routes(Some("alice"), args.clone(), &[first, second])
                .await;
            for value in ["local", "Bob", "nobody"] {
                args["to"] = json!(value);
                f.assert_routes(Some("alice"), args.clone(), &[]).await;
            }
        }
        f.assert_routes(
            Some("alice"),
            json!({"kind": "message", "from": "alice", "to": "bob"}),
            &[first, second],
        )
        .await;
        if !split {
            f.assert_routes(
                Some("alice"),
                json!({"kind": "note", "to": "bob"}),
                &[first, second],
            )
            .await;
        }
        f.finish().await;
    }
}

#[tokio::test]
async fn list_message_actor_filters_preserve_legacy_and_typed_fallbacks() {
    // The admitted legacy positive is first, so removing fallback cannot be
    // mistaken for a failure to construct one of the malformed fixtures.
    let mut legacy = Fixture::new(false);
    let id = legacy
        .seed(Note::new("local", "message", "legacy").with_properties(
            json!({"direction": "inbound", "from": "old-sender", "to": "old-recipient"}),
        ))
        .await;
    for (field, value) in [("from", "old-sender"), ("to", "old-recipient")] {
        legacy
            .assert_routes(None, json!({"kind": "message", (field): value}), &[id])
            .await;
        legacy
            .assert_routes(
                Some("reader"),
                json!({"kind": "message", (field): value}),
                &[],
            )
            .await;
    }
    legacy.finish().await;

    let actor_shapes = [
        (None, "legacy-value"),
        (Some(Value::Null), "legacy-value"),
        (Some(json!(7)), "legacy-value"),
        (Some(json!(true)), "legacy-value"),
        (Some(json!(["actor"])), "legacy-value"),
        (Some(json!({"id": "actor"})), "legacy-value"),
        (Some(json!("actor")), "actor"),
        (Some(json!("")), ""),
        (Some(json!("  ")), "  "),
    ];
    for field in ["from", "to"] {
        let actor_field = format!("{field}_actor");
        for (actor_value, wanted) in &actor_shapes {
            let mut f = Fixture::new(false);
            let mut props = if field == "from" {
                json!({"direction": "inbound", "to_actor": "reader", "from": "legacy-value"})
            } else {
                json!({"direction": "outbound", "from_actor": "reader", "to": "legacy-value"})
            };
            if let Some(value) = actor_value {
                props[&actor_field] = value.clone();
            }
            let id = f
                .seed(Note::new("local", "message", "typed endpoint").with_properties(props))
                .await;
            f.assert_routes(
                Some("reader"),
                json!({"kind": "message", (field): wanted}),
                &[id],
            )
            .await;
            f.assert_routes(
                Some("reader"),
                json!({"kind": "message", (field): "local"}),
                &[],
            )
            .await;
            if *wanted != "legacy-value" {
                f.assert_routes(
                    Some("reader"),
                    json!({"kind": "message", (field): "legacy-value"}),
                    &[],
                )
                .await;
            }
            f.finish().await;
        }
        for legacy_value in [
            None,
            Some(Value::Null),
            Some(json!(12)),
            Some(json!(false)),
            Some(json!([])),
            Some(json!({})),
        ] {
            let mut f = Fixture::new(false);
            let mut props = if field == "from" {
                json!({"direction": "inbound", "to_actor": "reader"})
            } else {
                json!({"direction": "outbound", "from_actor": "reader"})
            };
            if let Some(value) = legacy_value {
                props[field] = value;
            }
            let id = f
                .seed(Note::new("local", "message", "no text endpoint").with_properties(props))
                .await;
            f.assert_routes(
                Some("reader"),
                json!({"kind": "message", (field): ""}),
                &[id],
            )
            .await;
            f.finish().await;
        }
    }

    let mut f = Fixture::new(false);
    let ordinary = f.seed(Note::new("local", "observation", "ordinary").with_properties(
        json!({"from": "legacy-from", "to": "legacy-to", "from_actor": "spoof-from", "to_actor": "spoof-to"}),
    )).await;
    for kind in ["note", "observation"] {
        for (field, legacy, actor) in [
            ("from", "legacy-from", "spoof-from"),
            ("to", "legacy-to", "spoof-to"),
        ] {
            f.assert_routes(
                Some("reader"),
                json!({"kind": kind, (field): legacy}),
                &[ordinary],
            )
            .await;
            f.assert_routes(Some("reader"), json!({"kind": kind, (field): actor}), &[])
                .await;
        }
    }
    f.finish().await;

    let mut f = Fixture::new(false);
    let missing = f.seed(Note::new("local", "message", "no properties")).await;
    let null = f
        .seed(Note::new("local", "message", "null properties").with_properties(Value::Null))
        .await;
    for field in ["from", "to"] {
        f.assert_routes(
            None,
            json!({"kind": "message", (field): ""}),
            &[missing, null],
        )
        .await;
        f.assert_routes(
            None,
            json!({"kind": "message", (field): Value::Null}),
            &[missing, null],
        )
        .await;
        f.assert_routes(Some("reader"), json!({"kind": "message", (field): ""}), &[])
            .await;
    }
    f.finish().await;
}

#[tokio::test]
async fn list_message_actor_filters_preserve_mailbox_and_stored_rows() {
    for split in [false, true] {
        let mut f = Fixture::new(split);
        let (out, incoming) = f.send("alice", "bob").await;
        let (_, other) = f.send("alice", "carol").await;
        for actor in ["bob", "observer"] {
            let expected = if actor == "bob" {
                vec![incoming]
            } else {
                vec![]
            };
            f.assert_routes(
                Some(actor),
                json!({"kind": "message", "from": "alice"}),
                &expected,
            )
            .await;
        }
        f.assert_routes(
            Some("alice"),
            json!({"kind": "message", "to": "bob"}),
            &[out],
        )
        .await;
        for actor in ["bob", "observer"] {
            for hidden in [out, other] {
                for mut args in [
                    json!({"kind": "message", "after": hidden.to_string()}),
                    json!({"kind": "message", "key_prefix": PREFIX, "after_key": f.row(hidden).note.key}),
                ] {
                    args["from"] = json!("alice");
                    let error = f.call(Some(actor), "list", args).await.unwrap_err();
                    assert!(
                        matches!(&error, RuntimeError::NotFound(_))
                            || matches!(&error, RuntimeError::Khive(error)
                                if error.kind() == khive_types::ErrorKind::NotFound),
                        "hidden anchor: {error}"
                    );
                }
            }
        }
        if split {
            let error = f
                .call(
                    Some("bob"),
                    "list",
                    json!({"kind": "note", "from": "alice"}),
                )
                .await
                .unwrap_err();
            assert!(error
                .to_string()
                .contains("message filters require kind=\"message\""));
            let token = f.main.authorize(Namespace::local()).unwrap();
            assert!(f
                .main
                .notes(&token)
                .unwrap()
                .get_note(incoming)
                .await
                .unwrap()
                .is_none());
        }
        f.finish().await;
    }

    let mut f = Fixture::new(false);
    for direction in ["inbound", "outbound"] {
        let owner_field = if direction == "inbound" {
            "to_actor"
        } else {
            "from_actor"
        };
        for value in [
            json!(5),
            json!(true),
            json!([]),
            json!({}),
            json!(""),
            json!("  "),
        ] {
            f.seed(Note::new("local", "message", "malformed owner").with_properties(
                json!({"direction": direction, (owner_field): value, "from": "legacy-from", "to": "legacy-to"}),
            )).await;
        }
    }
    for actor in [None, Some("reader")] {
        f.assert_routes(
            actor,
            json!({"kind": "message", "from": "legacy-from"}),
            &[],
        )
        .await;
        f.assert_routes(actor, json!({"kind": "message", "to": "legacy-to"}), &[])
            .await;
    }
    f.finish().await;
}
