//! Typed refusal metadata crosses actual registry dispatch and MCP projection.

use super::DispatchFailure;
use khive_runtime::{
    ChannelIngestFailureClass, HandlerDef, NamespaceToken, PackRuntime, RefusalEventRecording,
    RefusalRecordingErrorClass, RuntimeError, VerbCategory, VerbRegistry, VerbRegistryBuilder,
    Visibility,
};
use khive_types::Pack;
use serde_json::{json, Value};
use uuid::Uuid;

#[derive(Clone, Copy)]
enum Mode {
    Recorded,
    Failed,
    Unadorned,
    Lookalike,
}

struct RefusalFixture(Mode);

impl Pack for RefusalFixture {
    const NAME: &'static str = "refusal_fixture";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[HandlerDef {
        name: "refusal_fixture",
        description: "return a typed refusal with fixture recording evidence",
        visibility: Visibility::Verb,
        category: VerbCategory::Directive,
        params: &[],
    }];
}

#[async_trait::async_trait]
impl PackRuntime for RefusalFixture {
    fn name(&self) -> &str {
        <Self as Pack>::NAME
    }
    fn note_kinds(&self) -> &'static [&'static str] {
        <Self as Pack>::NOTE_KINDS
    }
    fn entity_kinds(&self) -> &'static [&'static str] {
        <Self as Pack>::ENTITY_KINDS
    }
    fn handlers(&self) -> &'static [HandlerDef] {
        <Self as Pack>::HANDLERS
    }

    async fn dispatch(
        &self,
        _verb: &str,
        _params: Value,
        _registry: &VerbRegistry,
        _token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        let source = RuntimeError::SecretDetected(khive_runtime::secret_gate::SecretMatch {
            detector: "fixture",
            trigger: None,
            masked: "PRIVATE-MASKED-EXCERPT".into(),
            location: Some("atoms[0].content".into()),
        });
        let source = if matches!(self.0, Mode::Lookalike) {
            RuntimeError::InvalidInput(source.to_string())
        } else {
            source
        };
        let mut records = vec![RefusalEventRecording::Recorded {
            item_index: 0,
            subject: Uuid::from_u128(11),
            event_id: Uuid::from_u128(12),
        }];
        match self.0 {
            Mode::Failed | Mode::Lookalike => records.push(RefusalEventRecording::Failed {
                item_index: 2,
                subject: Uuid::from_u128(13),
                error_class: RefusalRecordingErrorClass::EventAppendFailed,
            }),
            Mode::Unadorned => records.clear(),
            Mode::Recorded => {}
        }
        Err(source.with_refusal_events(records))
    }
}

async fn dispatch(mode: Mode) -> Value {
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some("lambda:refusal-fixture".into()));
    builder.with_default_namespace("lambda:refusal-fixture");
    builder.register(RefusalFixture(mode));
    // No runtime, storage, or ambient database config is needed for this seam.
    let registry = builder.build().unwrap();
    let error = registry
        .dispatch_with_disposition("refusal_fixture", json!({}), None)
        .await
        .expect_err("fixture must refuse");
    if !matches!(mode, Mode::Lookalike) {
        assert_eq!(
            error.source().channel_ingest_failure_class(),
            ChannelIngestFailureClass::Permanent {
                reason: "SecretDetected"
            }
        );
    }
    DispatchFailure::from_dispatch("refusal_fixture", error).into_entry()
}

#[tokio::test]
async fn refusal_events_keep_mcp_gate_classification_for_recorded_and_failed_appends() {
    for (mode, recorded, count) in [(Mode::Recorded, true, 1), (Mode::Failed, false, 2)] {
        let entry = dispatch(mode).await;
        assert_eq!(entry["ok"], false);
        assert_eq!(entry["reason"], "gate-refusal");
        assert_eq!(entry["error"]["code"], "secret_detected");
        assert_eq!(entry["error"]["detector"], "fixture");
        assert_eq!(entry["error"]["location"], "atoms[0].content");
        assert_eq!(entry["error"]["refusal_recorded"], recorded);
        assert_eq!(
            entry["error"]["refusal_events"].as_array().unwrap().len(),
            count
        );
        assert_eq!(entry["error"]["domain_disposition"], "unknown");
        assert_eq!(entry["domain_disposition"], "unknown");
        assert!(!entry.to_string().contains("PRIVATE-MASKED-EXCERPT"));
        if !recorded {
            assert_eq!(
                entry["error"]["refusal_events"][1]["error_class"],
                "event_append_failed"
            );
            assert!(entry["error"]["refusal_events"][1]
                .get("event_id")
                .is_none());
        }
    }
}

#[tokio::test]
async fn refusal_events_neither_decorate_ineligible_targets_nor_promote_message_lookalikes() {
    let bare = dispatch(Mode::Unadorned).await;
    assert_eq!(bare["reason"], "gate-refusal");
    assert!(bare["error"].get("refusal_recorded").is_none());
    assert!(bare["error"].get("refusal_events").is_none());
    let lookalike = dispatch(Mode::Lookalike).await;
    assert!(lookalike.get("reason").is_none());
    assert!(lookalike["error"].get("code").is_none());
    assert_eq!(lookalike["error"]["refusal_recorded"], false);
}
