//! `CommPack` struct, `Pack` impl, `PackRuntime` impl, and self-registration.

use async_trait::async_trait;
use serde_json::Value;

use khive_runtime::pack::PackRuntime;
use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError, SchemaPlan, VerbRegistry};
use khive_types::{HandlerDef, Pack};

use crate::handlers;
use crate::vocab::{COMM_HANDLERS, COMM_SCHEMA_PLAN_STMTS};

/// Communication pack providing the public `comm.*` verbs and channel subhandlers.
///
/// Stores and queries `message` notes in the standard notes table; message
/// metadata lives in the `properties` JSON column.
pub struct CommPack {
    runtime: KhiveRuntime,
}

impl Pack for CommPack {
    const NAME: &'static str = "comm";
    const NOTE_KINDS: &'static [&'static str] = &["message", "channel_health"];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &COMM_HANDLERS;
    const REQUIRES: &'static [&'static str] = &["kg"];
}

impl CommPack {
    /// Create a new `CommPack` bound to the given runtime.
    pub fn new(runtime: KhiveRuntime) -> Self {
        Self { runtime }
    }
    pub(crate) fn runtime(&self) -> &KhiveRuntime {
        &self.runtime
    }
}

struct CommPackFactory;

impl khive_runtime::PackFactory for CommPackFactory {
    fn name(&self) -> &'static str {
        "comm"
    }
    fn requires(&self) -> &'static [&'static str] {
        &["kg"]
    }
    fn create(&self, runtime: KhiveRuntime) -> Box<dyn khive_runtime::PackRuntime> {
        Box::new(CommPack::new(runtime))
    }
}

inventory::submit! { khive_runtime::PackRegistration(&CommPackFactory) }

#[async_trait]
impl PackRuntime for CommPack {
    fn name(&self) -> &str {
        <CommPack as Pack>::NAME
    }
    fn note_kinds(&self) -> &'static [&'static str] {
        <CommPack as Pack>::NOTE_KINDS
    }
    fn entity_kinds(&self) -> &'static [&'static str] {
        <CommPack as Pack>::ENTITY_KINDS
    }
    fn handlers(&self) -> &'static [HandlerDef] {
        &COMM_HANDLERS
    }
    fn requires(&self) -> &'static [&'static str] {
        <CommPack as Pack>::REQUIRES
    }

    fn schema_plan(&self) -> SchemaPlan {
        SchemaPlan {
            pack: "comm",
            statements: &COMM_SCHEMA_PLAN_STMTS,
        }
    }

    async fn dispatch(
        &self,
        verb: &str,
        params: Value,
        _registry: &VerbRegistry,
        token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        match verb {
            "comm.send" => handlers::handle_send(self.runtime(), token, params).await,
            "comm.delivered" => handlers::handle_delivered(self.runtime(), token, params).await,
            "comm.inbox" => handlers::handle_inbox(self.runtime(), token, params).await,
            "comm.read" => handlers::handle_read(self.runtime(), token, params).await,
            "comm.unread" => handlers::handle_unread(self.runtime(), token, params).await,
            "comm.reply" => handlers::handle_reply(self.runtime(), token, params).await,
            "comm.thread" => handlers::handle_thread(self.runtime(), token, params).await,
            "comm.ingest" => handlers::handle_ingest(self.runtime(), token, params).await,
            "comm.heartbeat" => handlers::handle_heartbeat(self.runtime(), token, params).await,
            "comm.health" => handlers::handle_health(self.runtime(), token, params).await,
            "comm.probe" => handlers::handle_probe(self.runtime(), token, params).await,
            "comm.cursor_get" => handlers::handle_cursor_get(self.runtime(), params).await,
            "comm.cursor_commit" => handlers::handle_cursor_commit(self.runtime(), params).await,
            _ => Err(RuntimeError::InvalidInput(format!(
                "comm pack does not handle verb {verb:?}"
            ))),
        }
    }
}

#[cfg(test)]
mod help_tests {
    use super::*;
    use khive_types::{Pack, VerbCategory, Visibility};

    fn find_handler(name: &str) -> &'static HandlerDef {
        CommPack::HANDLERS
            .iter()
            .find(|h| h.name == name)
            .unwrap_or_else(|| panic!("handler {name:?} not found in comm pack"))
    }

    #[test]
    fn send_has_required_to_and_content() {
        let h = find_handler("comm.send");
        assert!(!h.params.is_empty(), "send must have non-empty params");
        let to = h
            .params
            .iter()
            .find(|p| p.name == "to")
            .expect("send must have 'to'");
        assert!(to.required, "send.to must be required");
        let content = h
            .params
            .iter()
            .find(|p| p.name == "content")
            .expect("send must have 'content'");
        assert!(content.required, "send.content must be required");
    }

    #[test]
    fn send_has_optional_subject_and_thread_id() {
        let h = find_handler("comm.send");
        let subject = h
            .params
            .iter()
            .find(|p| p.name == "subject")
            .expect("send must have 'subject'");
        assert!(!subject.required, "send.subject must be optional");
        let thread_id = h
            .params
            .iter()
            .find(|p| p.name == "thread_id")
            .expect("send must have 'thread_id'");
        assert!(!thread_id.required, "send.thread_id must be optional");
    }

    #[test]
    fn send_declares_optional_self_send_contract() {
        let h = find_handler("comm.send");
        let self_send = h
            .params
            .iter()
            .find(|p| p.name == "self_send")
            .expect("comm.send help must declare 'self_send'");

        assert_eq!(self_send.param_type, "boolean");
        assert!(!self_send.required, "send.self_send must be optional");
        assert!(
            self_send.description.contains("Defaults to false"),
            "send.self_send help must document its default"
        );
        assert!(
            self_send
                .description
                .contains("matches the configured sender actor")
                && self_send.description.contains("`local`"),
            "send.self_send help must document when the opt-in is required"
        );
    }

    #[test]
    fn inbox_has_optional_limit_and_status() {
        let h = find_handler("comm.inbox");
        assert!(!h.params.is_empty(), "inbox must have non-empty params");
        let limit = h
            .params
            .iter()
            .find(|p| p.name == "limit")
            .expect("inbox must have 'limit'");
        assert!(!limit.required, "inbox.limit must be optional");
        let status = h
            .params
            .iter()
            .find(|p| p.name == "status")
            .expect("inbox must have 'status'");
        assert!(!status.required, "inbox.status must be optional");
    }

    #[test]
    fn list_reads_declare_sent_box_and_shared_projection_contract() {
        let inbox = find_handler("comm.inbox");
        for name in ["box", "to_actor", "fields"] {
            let param = inbox
                .params
                .iter()
                .find(|param| param.name == name)
                .unwrap_or_else(|| panic!("comm.inbox help must declare {name:?}"));
            assert!(!param.required, "comm.inbox.{name} must be optional");
        }

        let thread = find_handler("comm.thread");
        let fields = thread
            .params
            .iter()
            .find(|param| param.name == "fields")
            .expect("comm.thread help must declare the shared fields projection");
        assert_eq!(fields.param_type, "array of string");
        assert!(!fields.required);
        assert!(fields.description.contains("comm.inbox"));
    }

    #[test]
    fn read_has_optional_id_and_ids_for_exactly_one_validation() {
        let h = find_handler("comm.read");
        assert!(!h.params.is_empty(), "read must have non-empty params");
        let id = h
            .params
            .iter()
            .find(|p| p.name == "id")
            .expect("read must have 'id'");
        assert!(
            !id.required,
            "read.id is conditionally required with read.ids"
        );
        let ids = h
            .params
            .iter()
            .find(|p| p.name == "ids")
            .expect("read must have 'ids'");
        assert!(
            !ids.required,
            "read.ids is conditionally required with read.id"
        );
    }

    #[test]
    fn delivered_has_full_uuid_contract() {
        let h = find_handler("comm.delivered");
        assert_eq!(h.visibility, Visibility::Verb);
        assert_eq!(h.category, VerbCategory::Assertive);
        let id = h
            .params
            .iter()
            .find(|p| p.name == "id")
            .expect("delivered must have 'id'");
        assert!(id.required, "delivered.id must be required");
        assert_eq!(id.param_type, "uuid");
        assert!(
            h.description.contains("not external transport"),
            "delivered help must distinguish internal confirmation from SMTP delivery"
        );
    }

    /// #93 instance 3: the top-level description must not contradict `actor`
    /// being a required, non-inferred param — a caller reading only the
    /// one-line description (not the full param list) must not be misled
    /// into calling `comm.probe()` the way it can call `comm.inbox()`.
    #[test]
    fn probe_description_documents_required_actor() {
        let h = find_handler("comm.probe");
        assert!(
            h.description.contains("actor") && h.description.contains("required"),
            "comm.probe's top-level description must state that `actor` is \
             required and not inferred from the caller (issue #93); got: {:?}",
            h.description
        );
        let actor = h
            .params
            .iter()
            .find(|p| p.name == "actor")
            .expect("probe must have 'actor'");
        assert!(actor.required, "probe.actor must be required");
    }

    #[test]
    fn reply_has_required_id_and_content() {
        let h = find_handler("comm.reply");
        assert!(!h.params.is_empty(), "reply must have non-empty params");
        let id = h
            .params
            .iter()
            .find(|p| p.name == "id")
            .expect("reply must have 'id'");
        assert!(id.required, "reply.id must be required");
        let content = h
            .params
            .iter()
            .find(|p| p.name == "content")
            .expect("reply must have 'content'");
        assert!(content.required, "reply.content must be required");
    }

    #[test]
    fn all_comm_handlers_have_non_empty_params() {
        // comm.health and caller-scoped comm.unread are legitimate no-args verbs.
        const NO_ARGS_VERBS: &[&str] = &["comm.health", "comm.unread"];
        for handler in CommPack::HANDLERS {
            if NO_ARGS_VERBS.contains(&handler.name) {
                assert!(
                    handler.params.is_empty(),
                    "comm handler {:?} is declared no-args; params should be empty",
                    handler.name
                );
                continue;
            }
            assert!(
                !handler.params.is_empty(),
                "comm handler {:?} must have non-empty params",
                handler.name
            );
        }
    }

    /// COMM-AUD-001: verb categories must match speech-act classifications.
    #[test]
    fn verb_categories_match_spec() {
        let h = |name: &str| -> &'static HandlerDef {
            CommPack::HANDLERS
                .iter()
                .find(|h| h.name == name)
                .unwrap_or_else(|| panic!("handler {name:?} not found"))
        };
        assert_eq!(
            h("comm.send").category,
            VerbCategory::Commissive,
            "comm.send must be Commissive"
        );
        assert_eq!(
            h("comm.inbox").category,
            VerbCategory::Assertive,
            "comm.inbox must be Assertive"
        );
        assert_eq!(
            h("comm.delivered").category,
            VerbCategory::Assertive,
            "comm.delivered must be Assertive"
        );
        assert_eq!(
            h("comm.read").category,
            VerbCategory::Declaration,
            "comm.read must be Declaration"
        );
        assert_eq!(
            h("comm.reply").category,
            VerbCategory::Commissive,
            "comm.reply must be Commissive"
        );
        assert_eq!(
            h("comm.thread").category,
            VerbCategory::Assertive,
            "comm.thread must be Assertive"
        );
        assert_eq!(
            h("comm.ingest").category,
            VerbCategory::Declaration,
            "comm.ingest must be Declaration"
        );
    }

    #[test]
    fn ingest_is_subhandler_not_visible_on_wire() {
        let h = CommPack::HANDLERS
            .iter()
            .find(|h| h.name == "comm.ingest")
            .expect("comm.ingest must be declared");
        assert_eq!(
            h.visibility,
            Visibility::Subhandler,
            "comm.ingest must be Visibility::Subhandler — not callable on MCP wire"
        );
    }

    #[test]
    fn ingest_declares_required_namespace_param() {
        let h = CommPack::HANDLERS
            .iter()
            .find(|h| h.name == "comm.ingest")
            .expect("comm.ingest must be declared");
        let ns_param = h
            .params
            .iter()
            .find(|p| p.name == "namespace")
            .expect("comm.ingest must declare 'namespace' param");
        assert!(
            ns_param.required,
            "comm.ingest namespace param must be required so dispatch forwards it to the handler"
        );
    }

    #[test]
    fn ingest_declares_dedup_and_correlation_params() {
        let h = CommPack::HANDLERS
            .iter()
            .find(|h| h.name == "comm.ingest")
            .expect("comm.ingest must be declared");
        assert!(
            h.params.iter().any(|p| p.name == "external_id"),
            "comm.ingest must declare external_id param for deduplication"
        );
        assert!(
            h.params.iter().any(|p| p.name == "correlation_external_id"),
            "comm.ingest must declare correlation_external_id param for thread resolution"
        );
        assert!(
            h.params.iter().any(|p| p.name == "wire_message_id"),
            "comm.ingest must declare wire_message_id param (IngestParams carries it; \
             metadata must stay in sync per issue #403)"
        );
        assert!(
            h.params.iter().any(|p| p.name == "wire_references"),
            "comm.ingest must declare wire_references param (IngestParams carries it; \
             metadata must stay in sync per issue #403)"
        );
    }

    #[test]
    fn ingest_declares_optional_metadata_param() {
        // ADR-084 Rule 3 (help fidelity): comm.ingest's IngestParams.metadata
        // field (issue #448, quarantine marker passthrough) must be
        // reflected in the ParamDef/help schema, not just the Rust struct.
        let h = CommPack::HANDLERS
            .iter()
            .find(|h| h.name == "comm.ingest")
            .expect("comm.ingest must be declared");
        let metadata_param = h
            .params
            .iter()
            .find(|p| p.name == "metadata")
            .expect("comm.ingest must declare 'metadata' param");
        assert!(
            !metadata_param.required,
            "metadata must be optional so absent metadata preserves today's behavior exactly"
        );
    }

    #[test]
    fn ingest_declares_required_content_params() {
        let h = CommPack::HANDLERS
            .iter()
            .find(|h| h.name == "comm.ingest")
            .expect("comm.ingest must be declared");
        for required_name in &["from", "to", "content"] {
            let p = h
                .params
                .iter()
                .find(|p| p.name == *required_name)
                .unwrap_or_else(|| {
                    panic!("comm.ingest must declare required param '{required_name}'")
                });
            assert!(
                p.required,
                "comm.ingest param '{required_name}' must be required"
            );
        }
    }
}
