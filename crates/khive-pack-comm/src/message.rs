//! Core message primitives: ID helpers and the dual-write delivery function.

use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::{micros_to_iso, KhiveRuntime, Namespace, NamespaceToken, RuntimeError};
use khive_storage::{note::Note, StorageError, WriterTaskRequestState};
use khive_types::{Details, KhiveError};

pub(crate) const COMM_SCHEMA_VERSION: u64 = 1;
pub(crate) const COMM_STABLE_PROPERTY_KEYS: &[&str] = &[
    "comm_schema_version",
    "direction",
    "read",
    "from_actor",
    "to_actor",
    "from",
    "to",
    "thread_id",
    "subject",
    "sent_at",
    "outbound_ref",
    "sent_by_process",
];

/// Closed field vocabulary accepted by list-read message projections.
///
/// The first group is the ordinary top-level message view. The second group
/// exposes stable property keys as top-level aliases only when a caller opts
/// into projection, so callers can request routing/timestamp metadata without
/// paying for the entire `properties` object.
pub(crate) const MESSAGE_PROJECTION_FIELDS: &[&str] = &[
    "id",
    "short_id",
    "full_id",
    "kind",
    "from",
    "to",
    "subject",
    "read",
    "direction",
    "preview",
    "content",
    "namespace",
    "properties",
    "created_at",
    "updated_at",
    "comm_schema_version",
    "from_actor",
    "to_actor",
    "thread_id",
    "sent_at",
    "outbound_ref",
    "sent_by_process",
];

pub(crate) fn validate_message_projection_fields(
    verb: &str,
    fields: Option<&[String]>,
) -> Result<(), RuntimeError> {
    let Some(fields) = fields else {
        return Ok(());
    };
    if fields.is_empty() {
        return Err(RuntimeError::InvalidInput(format!(
            "{verb}: `fields` must contain at least one field"
        )));
    }
    if let Some(unknown) = fields
        .iter()
        .find(|field| !MESSAGE_PROJECTION_FIELDS.contains(&field.as_str()))
    {
        return Err(RuntimeError::InvalidInput(format!(
            "{verb}: unknown projection field {unknown:?}; expected one of: {}",
            MESSAGE_PROJECTION_FIELDS.join(", ")
        )));
    }
    Ok(())
}

pub(crate) fn project_message_json(message: Value, fields: Option<&[String]>) -> Value {
    let Some(fields) = fields else {
        return message;
    };

    let mut projected = serde_json::Map::new();
    for field in fields {
        let value = message.get(field).cloned().unwrap_or_else(|| {
            message
                .get("properties")
                .and_then(|properties| properties.get(field))
                .cloned()
                .or_else(|| match field.as_str() {
                    "from_actor" => message.get("from").cloned(),
                    "to_actor" => message.get("to").cloned(),
                    _ => None,
                })
                .unwrap_or(Value::Null)
        });
        projected.insert(field.clone(), value);
    }
    Value::Object(projected)
}

pub(crate) fn short_id(uuid: Uuid) -> String {
    uuid.as_hyphenated().to_string().chars().take(8).collect()
}

/// Resolve a raw id string (full UUID or 8+ hex-char short prefix) to a UUID.
pub(crate) async fn resolve_id(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    raw: &str,
    verb: &str,
) -> Result<Uuid, RuntimeError> {
    if let Ok(uuid) = raw.parse::<Uuid>() {
        return Ok(uuid);
    }
    if raw.len() >= 8 && raw.chars().all(|c| c.is_ascii_hexdigit()) {
        return match runtime.resolve_prefix(token, raw).await? {
            Some(uuid) => Ok(uuid),
            None => Err(RuntimeError::InvalidInput(format!(
                "{verb}: no record matches prefix: {raw:?}"
            ))),
        };
    }
    Err(RuntimeError::InvalidInput(format!(
        "{verb}: invalid id {raw:?}; expected full UUID or 8-char hex prefix"
    )))
}

fn attach_outbound_id_to_ambiguous_write(outbound_id: Uuid, error: RuntimeError) -> RuntimeError {
    match error {
        RuntimeError::Storage(StorageError::WriterTaskTerminated {
            request_state: WriterTaskRequestState::SideEffectsUnknown,
        }) => RuntimeError::Khive(
            KhiveError::conflict(format!(
                "dual_write delivery outcome is uncertain (side_effects_unknown); \
                 call comm.delivered(id=\"{outbound_id}\") before retrying"
            ))
            .with_details(Details::new_owned([(
                "outbound_id",
                outbound_id.to_string(),
            )])),
        ),
        other => other,
    }
}

pub(crate) fn note_to_message_json(note: &Note) -> Value {
    let props = note.properties.as_ref();
    let full_id = note.id.as_hyphenated().to_string();

    let from = props
        .and_then(|p| p.get("from_actor"))
        .and_then(Value::as_str)
        .map(|s| Value::String(s.to_string()))
        .unwrap_or_else(|| Value::String(note.namespace.clone()));

    let to = props
        .and_then(|p| p.get("to_actor"))
        .cloned()
        .unwrap_or(Value::Null);

    let subject = props
        .and_then(|p| p.get("subject"))
        .cloned()
        .unwrap_or(Value::Null);

    let read = props
        .and_then(|p| p.get("read"))
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let direction = props
        .and_then(|p| p.get("direction"))
        .cloned()
        .unwrap_or(Value::Null);

    let preview = build_preview(&note.content);

    json!({
        // `id` is the round-trippable identifier. Keep `full_id` as a
        // compatibility alias and expose the ambiguous prefix only as an
        // explicitly display-oriented field (#1421).
        "id": full_id.clone(),
        "short_id": short_id(note.id),
        "full_id": full_id,
        "kind": "message",
        "from": from,
        "to": to,
        "subject": subject,
        "read": read,
        "direction": direction,
        "preview": preview,
        "content": note.content,
        "namespace": note.namespace,
        "properties": note.properties,
        "created_at": micros_to_iso(note.created_at),
        "updated_at": micros_to_iso(note.updated_at),
    })
}

fn build_preview(content: &str) -> String {
    const MAX_CHARS: usize = 80;
    let collapsed: String = content.split_whitespace().collect::<Vec<&str>>().join(" ");
    if collapsed.chars().count() > MAX_CHARS {
        let truncated: String = collapsed.chars().take(MAX_CHARS).collect();
        format!("{truncated}\u{2026}")
    } else {
        collapsed
    }
}

/// Writes an outbound copy (caller namespace) and an inbound copy (recipient
/// namespace) as ONE atomic unit
/// (`khive_runtime::create_notes_atomic_with_report`): one writer transaction
/// covers both notes' rows, FTS documents, and vector rows. Returns the
/// outbound `Note` and aggregate embedding-truncation report on success.
///
/// Resolved gap (external desk review, 2026-07-21; closed by construction
/// here): the two note writes used to be separate `create_note` calls with
/// only an in-process rollback compensating an inbound-write failure, so a
/// process crash between them could leave a durable orphan outbound note
/// with no inbound copy. `create_notes_atomic_with_report` commits both copies under one
/// `SqlAccess::atomic_unit` — a crash or failure anywhere in the unit rolls
/// back everything, so no partial pair can ever be observed durably: an
/// ordinary prepare/plan failure leaves neither copy, while a successful
/// commit leaves both. If the writer seam cannot establish whether an
/// accepted request committed (`side_effects_unknown`), the pre-generated
/// outbound UUID is attached to the error so `comm.delivered` can distinguish
/// the two complete outcomes without relying on message content.
///
/// Invariant: the outbound id is generated BEFORE either write (rather than
/// patched in after, as the old two-call version did) so both copies carry
/// the canonical `thread_id` AND `comm_schema_version` from their first
/// write: for a root send (`thread_id: None`), that id IS the canonical
/// thread_id; for a reply, the caller-supplied `thread_id` is forwarded
/// unchanged. Either way there is no separate patch write, and no row is
/// ever durably observable with a canonical thread_id but no version marker
/// (or vice versa) — `create_notes_atomic` commits both in the same
/// transaction as the row itself.
///
/// See crates/khive-pack-comm/docs/api/message-lifecycle.md#messagersdual_write_message for
/// the `in_reply_to_message_id`/`references_chain` header-threading contract.
// REASON: dual_write_message mirrors the send wire shape plus its persisted metadata and the
// two context args (runtime, token). Grouping them into a struct would not reduce overall
// complexity and would require an extra allocation on the hot path; the flat signature is
// intentional.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn dual_write_message(
    runtime: &KhiveRuntime,
    caller_token: &NamespaceToken,
    from: &str,
    to: &str,
    subject: Option<&str>,
    content: &str,
    thread_id: Option<&str>,
    sent_at: &str,
    sent_by_process: Option<&str>,
    from_actor: Option<&str>,
    to_actor: Option<&str>,
    in_reply_to_message_id: Option<&str>,
    references_chain: Option<&str>,
    tags: Option<&[String]>,
) -> Result<(Note, khive_runtime::retrieval::EmbeddingTruncationReport), RuntimeError> {
    let recipient_ns_str = to.trim();
    if from != recipient_ns_str {
        // When actor labels are provided this is an actor-addressed local send;
        // both copies land in the caller's namespace so no cross-namespace check applies.
        // Only run the cross-namespace gate when no actor routing is in use.
        if from_actor.is_none() {
            // 1. Validate recipient namespace string format first.
            let recipient_ns = match Namespace::parse(recipient_ns_str) {
                Ok(ns) => ns,
                Err(e) => {
                    return Err(RuntimeError::InvalidInput(format!(
                        "send: invalid recipient namespace {to:?}: {e}"
                    )));
                }
            };

            // 2. Check sender-side outbound allowlist from config.
            //    Cross-namespace delivery is permitted only for declared recipients.
            let allowed = runtime
                .config()
                .allowed_outbound_namespaces
                .iter()
                .any(|ns| ns == &recipient_ns);

            if !allowed {
                return Err(RuntimeError::PermissionDenied {
                    verb: "comm.send".to_string(),
                    reason: format!(
                        "cross-namespace delivery to {recipient_ns_str:?} is not permitted; \
                         add {recipient_ns_str:?} to actor.allowed_outbound_namespaces in \
                         the sender's khive.toml to enable delivery"
                    ),
                });
            }
            // 3. Allowlist hit: fall through to outbound note creation.
        }
    }

    // Pre-generate the outbound id so the canonical thread_id is known before
    // any write is attempted — both copies carry it from their first write,
    // eliminating the separate thread_id-patch transaction the two-call
    // version needed for root sends.
    let outbound_id = Uuid::new_v4();
    let canonical_thread_id: String = match thread_id {
        Some(tid) => tid.to_string(),
        None => outbound_id.as_hyphenated().to_string(),
    };

    let mut outbound_props = json!({
        "comm_schema_version": COMM_SCHEMA_VERSION,
        "from": from,
        "to": to,
        "direction": "outbound",
        "subject": subject,
        "thread_id": canonical_thread_id,
        "read": false,
        "sent_at": sent_at,
    });
    if let Some(fa) = from_actor {
        outbound_props["from_actor"] = json!(fa);
    }
    if let Some(ta) = to_actor {
        outbound_props["to_actor"] = json!(ta);
    }
    if let Some(process_ref) = sent_by_process {
        outbound_props["sent_by_process"] = json!(process_ref);
    }
    if let Some(irt) = in_reply_to_message_id {
        outbound_props["in_reply_to_message_id"] = json!(irt);
    }
    if let Some(refs) = references_chain {
        outbound_props["references_chain"] = json!(refs);
    }
    if let Some(t) = tags {
        if !t.is_empty() {
            outbound_props["tags"] = json!(t);
        }
    }

    // When actor labels are provided (ADR-057 actor-addressed path), both copies
    // land in the caller's namespace — no cross-namespace write occurs.
    // When sender and recipient are in different namespaces (allowed cross-ns path),
    // mint a recipient-scoped read+write token used for the inbound write after the
    // allowlist check so the inbound note lands in the correct inbox. For
    // same-namespace sends (from == to), use caller_token unchanged (preserves
    // existing behavior).
    let cross_ns_token;
    let inbound_tok: &NamespaceToken = if from_actor.is_some() || from == recipient_ns_str {
        // Actor-addressed path or same-namespace send: inbound copy stays in caller ns.
        caller_token
    } else {
        cross_ns_token = caller_token.with_namespace(
            Namespace::parse(recipient_ns_str).expect("recipient_ns_str already validated above"),
        );
        &cross_ns_token
    };

    let mut inbound_props = json!({
        "comm_schema_version": COMM_SCHEMA_VERSION,
        "from": from,
        "to": to,
        "direction": "inbound",
        "subject": subject,
        "thread_id": canonical_thread_id,
        "read": false,
        "sent_at": sent_at,
        "outbound_ref": outbound_id,
    });
    if let Some(fa) = from_actor {
        inbound_props["from_actor"] = json!(fa);
    }
    if let Some(ta) = to_actor {
        inbound_props["to_actor"] = json!(ta);
    }
    if let Some(process_ref) = sent_by_process {
        inbound_props["sent_by_process"] = json!(process_ref);
    }
    if let Some(irt) = in_reply_to_message_id {
        inbound_props["in_reply_to_message_id"] = json!(irt);
    }
    if let Some(refs) = references_chain {
        inbound_props["references_chain"] = json!(refs);
    }
    if let Some(t) = tags {
        if !t.is_empty() {
            inbound_props["tags"] = json!(t);
        }
    }

    let (mut notes, embedding_truncation) = khive_runtime::create_notes_atomic_with_report(
        runtime,
        vec![
            khive_runtime::AtomicNoteSpec {
                token: caller_token,
                id: Some(outbound_id),
                kind: "message",
                name: subject,
                content,
                properties: Some(outbound_props),
            },
            khive_runtime::AtomicNoteSpec {
                token: inbound_tok,
                id: None,
                kind: "message",
                name: subject,
                content,
                properties: Some(inbound_props),
            },
        ],
    )
    .await
    .map_err(|error| attach_outbound_id_to_ambiguous_write(outbound_id, error))?;

    // create_notes_atomic_with_report returns notes in the same order as the
    // specs above: [outbound, inbound].
    Ok((notes.remove(0), embedding_truncation))
}

#[cfg(test)]
mod tests {
    use super::*;
    use khive_storage::note::Note;
    use serde_json::json;

    // Issue #460 / atomic follow-up: dual_write_message must not leave a live
    // outbound note behind when the inbound copy's write fails. Since the
    // send-single-txn change, both copies commit under ONE atomic unit, so a
    // failure on the inbound copy's FTS statement (armed via
    // `arm_fts_fail_scoped(&recipient_ns)`, now consumed by
    // `create_notes_atomic` through `consume_fts_fail_fault`) rolls back the
    // WHOLE unit — including the outbound copy's already-applied statements
    // in sender_ns. Assert absence positively in BOTH namespaces rather than
    // inspecting the error string.
    #[tokio::test]
    async fn dual_write_inbound_failure_rolls_back_whole_unit_including_outbound() {
        use khive_runtime::{arm_fts_fail_scoped, AllowAllGate, BackendId, RuntimeConfig};

        let sender_ns = format!("t460-sender-{}", Uuid::new_v4().simple());
        let recipient_ns = format!("t460-recipient-{}", Uuid::new_v4().simple());

        let runtime = KhiveRuntime::new(RuntimeConfig {
            git_write: Default::default(),
            db_path: None,
            default_namespace: Namespace::parse(&sender_ns).unwrap(),
            embedding_model: None,
            additional_embedding_models: vec![],
            gate: std::sync::Arc::new(AllowAllGate),
            packs: vec!["kg".to_string(), "comm".to_string()],
            backend_id: BackendId::main(),
            brain_profile: None,
            visible_namespaces: vec![],
            allowed_outbound_namespaces: vec![Namespace::parse(&recipient_ns).unwrap()],
            actor_id: None,
        })
        .expect("in-memory runtime");

        let caller_token = runtime
            .authorize(Namespace::parse(&sender_ns).unwrap())
            .expect("authorize sender");
        let recipient_token = runtime
            .authorize(Namespace::parse(&recipient_ns).unwrap())
            .expect("authorize recipient");

        // Outbound copy targets sender_ns; inbound copy targets recipient_ns
        // and its FTS statement is armed to fail inside the atomic unit.
        let _fts_arm = arm_fts_fail_scoped(&recipient_ns);

        let result = dual_write_message(
            &runtime,
            &caller_token,
            &sender_ns,
            &recipient_ns,
            None,
            "F1 regression content",
            None,
            "2026-07-03T00:00:00Z",
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await;

        assert!(
            result.is_err(),
            "dual_write_message must fail when the inbound copy's write fails; got {result:?}"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("rolled back"),
            "error must report the atomic unit rolled back; got: {err_msg}"
        );
        for (token, ns) in [(&caller_token, "sender"), (&recipient_token, "recipient")] {
            let alive = runtime
                .list_notes(token, Some("message"), 100, 0)
                .await
                .expect("list_notes")
                .into_iter()
                .filter(|n| n.deleted_at.is_none())
                .count();
            assert_eq!(
                alive, 0,
                "no live note may remain in {ns}_ns after the whole unit rolled back; got {alive}"
            );
        }
    }

    /// Build a minimal same-namespace runtime + authorized token for the two
    /// tests below. The namespace is still generated fresh per call so
    /// `arm_vector_fail_scoped`/`arm_fts_fail_scoped` — process-wide,
    /// namespace-keyed statics — never race a concurrently-running test.
    fn scratch_runtime_and_token(ns: &str) -> (KhiveRuntime, NamespaceToken) {
        use khive_runtime::{AllowAllGate, BackendId, RuntimeConfig};

        let runtime = KhiveRuntime::new(RuntimeConfig {
            git_write: Default::default(),
            db_path: None,
            default_namespace: Namespace::parse(ns).unwrap(),
            embedding_model: None,
            additional_embedding_models: vec![],
            gate: std::sync::Arc::new(AllowAllGate),
            packs: vec!["kg".to_string(), "comm".to_string()],
            backend_id: BackendId::main(),
            brain_profile: None,
            visible_namespaces: vec![],
            allowed_outbound_namespaces: vec![],
            actor_id: None,
        })
        .expect("in-memory runtime");
        let token = runtime
            .authorize(Namespace::parse(ns).unwrap())
            .expect("authorize");
        (runtime, token)
    }

    /// A root send (no `thread_id`) must store the SAME canonical thread_id —
    /// the outbound note's own id — on BOTH copies from their first write,
    /// not patched in afterward. `created_at == updated_at` on both is the
    /// positive signal that no later UPDATE ever touched either row
    /// post-creation.
    #[tokio::test]
    async fn send_root_thread_id_is_canonical_without_patch_write() {
        let ns = format!("thread-id-canonical-{}", Uuid::new_v4().simple());
        let (runtime, token) = scratch_runtime_and_token(&ns);

        let (outbound_note, _) = dual_write_message(
            &runtime,
            &token,
            &ns,
            &ns,
            None,
            "root send thread id",
            None,
            "2026-08-01T00:00:00Z",
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .expect("dual_write_message succeeds");
        let outbound_full_id = outbound_note.id.as_hyphenated().to_string();

        let alive: Vec<_> = runtime
            .list_notes(&token, Some("message"), 100, 0)
            .await
            .expect("list_notes")
            .into_iter()
            .filter(|n| n.deleted_at.is_none())
            .collect();
        assert_eq!(alive.len(), 2, "expected outbound + inbound; got {alive:?}");

        for note in &alive {
            let thread_id = note
                .properties
                .as_ref()
                .and_then(|p| p.get("thread_id"))
                .and_then(|v| v.as_str())
                .expect("thread_id property present");
            assert_eq!(
                thread_id, outbound_full_id,
                "both copies must carry the outbound id as canonical thread_id from \
                 their first write; note {} had {thread_id}",
                note.id
            );
            assert_eq!(
                note.created_at, note.updated_at,
                "no post-creation patch write may have touched note {} \
                 (created_at must equal updated_at)",
                note.id
            );
        }
    }

    /// Failure injection mid-unit, proven across a CROSS-namespace send: the
    /// outbound plan (sender namespace) applies its statements before the
    /// inbound plan (recipient namespace) runs, so arming the fault on the
    /// recipient namespace only is what actually exercises "an
    /// already-applied first plan unwinds" — arming the SAME namespace for
    /// both copies (as same-namespace sends do) lets the one-shot fault be
    /// consumed by the outbound plan instead, proving nothing about rollback
    /// of prior work. Absence is asserted positively — `list_notes`,
    /// `fts_notes` row counts, `VectorStore::count`, and `ann_write_log` row
    /// counts — in BOTH namespaces, not by inspecting the error.
    #[tokio::test]
    async fn send_vector_failure_mid_unit_rolls_back_both_notes_and_their_vectors() {
        use async_trait::async_trait;
        use khive_runtime::{
            arm_vector_fail_scoped, AllowAllGate, BackendId, EmbedderProvider, RuntimeConfig,
        };
        use khive_storage::types::SqlValue;
        use khive_storage::SqlStatement;
        use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};

        const MODEL: &str = "message-vecfail-model";
        const DIMS: usize = 4;

        struct StubService;
        #[async_trait]
        impl EmbeddingService for StubService {
            async fn embed(
                &self,
                texts: &[String],
                _model: EmbeddingModel,
            ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
                Ok(texts.iter().map(|_| vec![0.5_f32; DIMS]).collect())
            }
            fn supports_model(&self, _model: EmbeddingModel) -> bool {
                true
            }
            fn name(&self) -> &'static str {
                MODEL
            }
        }
        struct StubProvider;
        #[async_trait]
        impl EmbedderProvider for StubProvider {
            fn name(&self) -> &str {
                MODEL
            }
            fn dimensions(&self) -> usize {
                DIMS
            }
            async fn build(
                &self,
            ) -> khive_runtime::RuntimeResult<std::sync::Arc<dyn EmbeddingService>> {
                Ok(std::sync::Arc::new(StubService))
            }
        }

        let sender_ns = format!("vecfail-sender-{}", Uuid::new_v4().simple());
        let recipient_ns = format!("vecfail-recipient-{}", Uuid::new_v4().simple());

        let runtime = KhiveRuntime::new(RuntimeConfig {
            git_write: Default::default(),
            db_path: None,
            default_namespace: Namespace::parse(&sender_ns).unwrap(),
            embedding_model: None,
            additional_embedding_models: vec![],
            gate: std::sync::Arc::new(AllowAllGate),
            packs: vec!["kg".to_string(), "comm".to_string()],
            backend_id: BackendId::main(),
            brain_profile: None,
            visible_namespaces: vec![],
            allowed_outbound_namespaces: vec![Namespace::parse(&recipient_ns).unwrap()],
            actor_id: None,
        })
        .expect("in-memory runtime");
        runtime.register_embedder(StubProvider);

        let sender_token = runtime
            .authorize(Namespace::parse(&sender_ns).unwrap())
            .expect("authorize sender");
        let recipient_token = runtime
            .authorize(Namespace::parse(&recipient_ns).unwrap())
            .expect("authorize recipient");

        // No from_actor: this is a cross-namespace send, so the inbound copy
        // lands in recipient_ns (not the caller's namespace). Arm the fault
        // on recipient_ns — the SECOND plan in send order — so the outbound
        // plan's statements are already applied when the failure hits.
        let _vec_arm = arm_vector_fail_scoped(&recipient_ns);

        let result = dual_write_message(
            &runtime,
            &sender_token,
            &sender_ns,
            &recipient_ns,
            None,
            "vector fail mid-unit",
            None,
            "2026-08-01T00:00:00Z",
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await;
        assert!(
            result.is_err(),
            "dual_write_message must fail when the vector-insert statement is injected \
             to fail; got {result:?}"
        );

        for (token, ns, label) in [
            (&sender_token, &sender_ns, "sender"),
            (&recipient_token, &recipient_ns, "recipient"),
        ] {
            let alive = runtime
                .list_notes(token, Some("message"), 100, 0)
                .await
                .expect("list_notes")
                .into_iter()
                .filter(|n| n.deleted_at.is_none())
                .count();
            assert_eq!(
                alive, 0,
                "no note may survive a mid-unit vector failure in {label}_ns; got {alive}"
            );

            let mut reader = runtime.sql().reader().await.expect("sql reader");
            let fts_count = reader
                .query_scalar(SqlStatement {
                    sql: "SELECT COUNT(*) FROM fts_notes WHERE namespace = ?1".to_string(),
                    params: vec![SqlValue::Text(ns.clone())],
                    label: Some("test-fts-count".to_string()),
                })
                .await
                .expect("fts count query");
            assert!(
                matches!(fts_count, Some(SqlValue::Integer(0))),
                "no FTS document may survive a mid-unit vector failure in {label}_ns; got {fts_count:?}"
            );

            let vs = runtime.vectors_for_model(token, MODEL).expect("vec store");
            assert_eq!(
                vs.count().await.expect("count"),
                0,
                "no vector row may survive a mid-unit vector failure in {label}_ns"
            );

            let ann_count = reader
                .query_scalar(SqlStatement {
                    sql: "SELECT COUNT(*) FROM ann_write_log \
                          WHERE namespace = ?1 AND embedding_model = ?2"
                        .to_string(),
                    params: vec![
                        SqlValue::Text(ns.clone()),
                        SqlValue::Text(MODEL.to_string()),
                    ],
                    label: Some("test-ann-log-count".to_string()),
                })
                .await
                .expect("ann_write_log count query");
            assert!(
                matches!(ann_count, Some(SqlValue::Integer(0))),
                "no ann_write_log row may survive a mid-unit vector failure in {label}_ns; got {ann_count:?}"
            );
        }
    }

    #[test]
    fn side_effects_unknown_surfaces_outbound_confirmation_id() {
        let outbound_id = Uuid::new_v4();
        let error = RuntimeError::Storage(StorageError::WriterTaskTerminated {
            request_state: WriterTaskRequestState::SideEffectsUnknown,
        });

        let annotated = attach_outbound_id_to_ambiguous_write(outbound_id, error);
        let RuntimeError::Khive(khive_error) = &annotated else {
            panic!("expected RuntimeError::Khive, got {annotated:?}");
        };
        assert_eq!(khive_error.kind(), khive_types::ErrorKind::Conflict);
        assert_eq!(khive_error.retry_hint(), khive_types::RetryHint::NoRetry);
        assert_eq!(
            khive_error
                .details()
                .and_then(|d| d.get("outbound_id"))
                .map(str::to_string),
            Some(outbound_id.to_string())
        );
        assert!(khive_error.message().contains("side_effects_unknown"));
        assert!(khive_error
            .message()
            .contains(&format!("comm.delivered(id=\"{outbound_id}\")")));
    }

    #[test]
    fn known_atomic_rollback_is_not_mislabeled_ambiguous() {
        let error = RuntimeError::Internal("atomic multi-note write rolled back at op 1".into());
        let annotated = attach_outbound_id_to_ambiguous_write(Uuid::new_v4(), error);

        assert!(matches!(&annotated, RuntimeError::Internal(_)));
        assert!(!annotated.to_string().contains("outbound_id="));
    }

    #[tokio::test]
    async fn dual_write_versions_both_copies_and_stamps_optional_process_provenance() {
        use khive_runtime::{AllowAllGate, BackendId, RuntimeConfig};

        let runtime = KhiveRuntime::new(RuntimeConfig {
            git_write: Default::default(),
            db_path: None,
            default_namespace: Namespace::local(),
            embedding_model: None,
            additional_embedding_models: vec![],
            gate: std::sync::Arc::new(AllowAllGate),
            packs: vec!["kg".to_string(), "comm".to_string()],
            backend_id: BackendId::main(),
            brain_profile: None,
            visible_namespaces: vec![],
            allowed_outbound_namespaces: vec![],
            actor_id: None,
        })
        .expect("in-memory runtime");
        let caller_token = runtime
            .authorize(Namespace::local())
            .expect("authorize local");

        dual_write_message(
            &runtime,
            &caller_token,
            "local",
            "local",
            None,
            "with process provenance",
            None,
            "2026-07-31T00:00:00Z",
            Some("worker/run:42"),
            Some("local"),
            Some("local"),
            None,
            None,
            None,
        )
        .await
        .expect("dual write with provenance");
        dual_write_message(
            &runtime,
            &caller_token,
            "local",
            "local",
            None,
            "without process provenance",
            None,
            "2026-07-31T00:00:01Z",
            None,
            Some("local"),
            Some("local"),
            None,
            None,
            None,
        )
        .await
        .expect("dual write without provenance");

        let notes = runtime
            .list_notes(&caller_token, Some("message"), 100, 0)
            .await
            .expect("list messages");
        for (content, expected_process) in [
            ("with process provenance", Some("worker/run:42")),
            ("without process provenance", None),
        ] {
            let matching: Vec<_> = notes
                .iter()
                .filter(|note| note.content == content)
                .collect();
            assert_eq!(matching.len(), 2, "one outbound and one inbound copy");

            let mut directions = Vec::new();
            for note in matching {
                let props = note.properties.as_ref().expect("message properties");
                assert_eq!(
                    props.get("comm_schema_version").and_then(Value::as_u64),
                    Some(COMM_SCHEMA_VERSION)
                );
                assert_eq!(
                    props.get("sent_by_process").and_then(Value::as_str),
                    expected_process
                );
                directions.push(
                    props
                        .get("direction")
                        .and_then(Value::as_str)
                        .expect("direction"),
                );
            }
            directions.sort_unstable();
            assert_eq!(directions, ["inbound", "outbound"]);
        }
    }

    fn make_note(namespace: &str, content: &str, props: Option<Value>) -> Note {
        let mut n = Note::new(namespace, "message", content);
        n.properties = props;
        n
    }

    #[test]
    fn promotes_from_to_subject_when_present() {
        let note = make_note(
            "local",
            "hello",
            Some(json!({
                "from_actor": "lambda:khive",
                "to_actor": "lambda:leo",
                "subject": "Status update",
                "direction": "inbound",
                "read": false,
            })),
        );
        let v = note_to_message_json(&note);
        assert_eq!(v["from"], json!("lambda:khive"));
        assert_eq!(v["to"], json!("lambda:leo"));
        assert_eq!(v["subject"], json!("Status update"));
        assert_eq!(v["direction"], json!("inbound"));
        assert_eq!(v["read"], json!(false));
        assert!(v["content"].is_string());
        assert!(v["properties"].is_object());
    }

    #[test]
    fn message_ids_are_round_trippable_with_separate_compact_display_id() {
        let note = make_note("local", "hello", None);
        let full_id = note.id.as_hyphenated().to_string();
        let v = note_to_message_json(&note);

        assert_eq!(v["id"], json!(full_id));
        assert_eq!(v["full_id"], v["id"]);
        assert_eq!(v["short_id"], json!(short_id(note.id)));
    }

    #[test]
    fn from_falls_back_to_namespace_when_from_actor_absent() {
        let note = make_note(
            "legacy-ns",
            "old message",
            Some(json!({ "to_actor": "lambda:leo" })),
        );
        let v = note_to_message_json(&note);
        assert_eq!(v["from"], json!("legacy-ns"));
    }

    #[test]
    fn preview_is_single_line_and_truncated_for_long_content() {
        let long_body = "word ".repeat(40);
        let note = make_note("local", long_body.trim(), None);
        let v = note_to_message_json(&note);
        let preview = v["preview"].as_str().expect("preview is a string");
        assert!(!preview.contains('\n'), "preview must be single-line");
        assert!(
            preview.ends_with('\u{2026}'),
            "long preview must end with ellipsis"
        );
        let without_ellipsis: &str = &preview[..preview.len() - '\u{2026}'.len_utf8()];
        assert!(
            without_ellipsis.chars().count() <= 80,
            "preview body must not exceed 80 chars before ellipsis"
        );
    }

    #[test]
    fn preview_not_truncated_for_short_content() {
        let note = make_note("local", "short message", None);
        let v = note_to_message_json(&note);
        let preview = v["preview"].as_str().expect("preview is a string");
        assert_eq!(preview, "short message");
        assert!(!preview.ends_with('\u{2026}'));
    }

    #[test]
    fn preview_collapses_whitespace_and_newlines() {
        let note = make_note("local", "line one\n  line two\n\nline three", None);
        let v = note_to_message_json(&note);
        let preview = v["preview"].as_str().expect("preview is a string");
        assert_eq!(preview, "line one line two line three");
    }

    #[test]
    fn properties_and_content_still_present() {
        let note = make_note(
            "local",
            "body text",
            Some(json!({ "from_actor": "x", "custom": 42 })),
        );
        let v = note_to_message_json(&note);
        assert_eq!(v["content"], json!("body text"));
        assert_eq!(v["properties"]["custom"], json!(42));
    }

    #[test]
    fn null_defaults_when_no_properties() {
        let note = make_note("local", "no props", None);
        let v = note_to_message_json(&note);
        assert_eq!(v["to"], Value::Null);
        assert_eq!(v["subject"], Value::Null);
        assert_eq!(v["direction"], Value::Null);
        assert_eq!(v["read"], json!(false));
        assert_eq!(v["from"], json!("local"));
    }
}
