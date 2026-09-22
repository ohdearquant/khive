//! #2047: authorization observes submitted, not hook-normalized arguments.
use super::*;
use khive_types::Pack;
use std::sync::Mutex;

#[derive(Debug, Default)]
struct RecordingGate(Mutex<Vec<Value>>);
impl khive_gate::Gate for RecordingGate {
    fn check(&self, req: &GateRequest) -> Result<GateDecision, khive_gate::GateError> {
        self.0.lock().unwrap().push(req.args.clone());
        Ok(GateDecision::allow())
    }
}

#[derive(Debug)]
struct RewriteHook;
#[async_trait]
impl KindHook for RewriteHook {
    async fn prepare_create(&self, _: &KhiveRuntime, _: &mut Value) -> Result<(), RuntimeError> {
        Ok(())
    }
    async fn after_create(
        &self,
        _: &KhiveRuntime,
        _: uuid::Uuid,
        _: &Value,
    ) -> Result<(), RuntimeError> {
        Ok(())
    }
    async fn normalize_note_update(
        &self,
        _: &KhiveRuntime,
        _: &NamespaceToken,
        _: &khive_storage::Note,
        args: &mut Value,
    ) -> Result<(), RuntimeError> {
        let submitted = args.as_object_mut().unwrap().remove("raw_marker").unwrap();
        args["properties"] = serde_json::json!({"marker": submitted});
        Ok(())
    }
}

struct RewritePack(KhiveRuntime);
impl Pack for RewritePack {
    const NAME: &'static str = "argument-contract-probe";
    const NOTE_KINDS: &'static [&'static str] = &["argument-contract-note"];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[HandlerDef {
        name: "argument_contract.update",
        description: "exercise post-authorization note hook",
        visibility: Visibility::Verb,
        category: VerbCategory::Commissive,
        params: &[],
    }];
}
#[async_trait]
impl PackRuntime for RewritePack {
    fn name(&self) -> &str {
        Self::NAME
    }
    fn note_kinds(&self) -> &'static [&'static str] {
        Self::NOTE_KINDS
    }
    fn entity_kinds(&self) -> &'static [&'static str] {
        Self::ENTITY_KINDS
    }
    fn handlers(&self) -> &'static [HandlerDef] {
        Self::HANDLERS
    }
    fn kind_hook(&self, kind: &str) -> Option<Arc<dyn KindHook>> {
        (kind == "argument-contract-note").then(|| Arc::new(RewriteHook) as Arc<dyn KindHook>)
    }
    async fn dispatch(
        &self,
        _: &str,
        mut params: Value,
        registry: &VerbRegistry,
        token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        let note = khive_storage::Note::new("local", "argument-contract-note", "body");
        registry
            .prepare_note_update_hook(&self.0, token, &note, &mut params)
            .await?;
        Ok(params)
    }
}

#[tokio::test]
async fn gate_sees_submitted_args_before_handler_kind_hook_rewrite() {
    let runtime = KhiveRuntime::memory().expect("runtime");
    let gate = Arc::new(RecordingGate::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.with_gate(gate.clone());
    builder.register(RewritePack(runtime));
    let registry = builder.build().expect("registry");
    let submitted = serde_json::json!({"raw_marker": true});
    let effective = registry
        .dispatch("argument_contract.update", submitted.clone())
        .await
        .expect("dispatch");
    let captured = gate.0.lock().unwrap();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0], submitted);
    assert_eq!(effective["properties"]["marker"], true);
    assert!(effective.get("raw_marker").is_none());
    assert_ne!(captured[0], effective);
}
