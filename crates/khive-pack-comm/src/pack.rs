//! `CommPack` struct, `Pack` impl, `PackRuntime` impl, and self-registration.

use async_trait::async_trait;
use serde_json::Value;

use khive_runtime::pack::PackRuntime;
use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError, SchemaPlan, VerbRegistry};
use khive_types::{HandlerDef, Pack};

use crate::handlers;
use crate::vocab::{COMM_HANDLERS, COMM_SCHEMA_PLAN_STMTS};

/// Communication pack providing the five `comm.*` verbs.
///
/// Stores and queries `message` notes in the standard notes table; message
/// metadata lives in the `properties` JSON column.
pub struct CommPack {
    runtime: KhiveRuntime,
}

impl Pack for CommPack {
    const NAME: &'static str = "comm";
    const NOTE_KINDS: &'static [&'static str] = &["message"];
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
            "comm.inbox" => handlers::handle_inbox(self.runtime(), token, params).await,
            "comm.read" => handlers::handle_read(self.runtime(), token, params).await,
            "comm.reply" => handlers::handle_reply(self.runtime(), token, params).await,
            "comm.thread" => handlers::handle_thread(self.runtime(), token, params).await,
            _ => Err(RuntimeError::InvalidInput(format!(
                "comm pack does not handle verb {verb:?}"
            ))),
        }
    }
}

#[cfg(test)]
mod help_tests {
    use super::*;
    use khive_types::{Pack, VerbCategory};

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
    fn read_has_required_id() {
        let h = find_handler("comm.read");
        assert!(!h.params.is_empty(), "read must have non-empty params");
        let id = h
            .params
            .iter()
            .find(|p| p.name == "id")
            .expect("read must have 'id'");
        assert!(id.required, "read.id must be required");
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
        for handler in CommPack::HANDLERS {
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
    }
}
