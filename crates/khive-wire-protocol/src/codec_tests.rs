use super::*;
use crate::frame::OperationId;

/// One representative frame per kind, with every optional field
/// present, for round-trip and closed-set controls.
fn sample_frames() -> Vec<Frame> {
    vec![
        Frame::Handshake {
            version: crate::version::CURRENT_VERSION,
        },
        Frame::HandshakeAck {
            version: crate::version::CURRENT_VERSION,
        },
        Frame::Request {
            id: OperationId::from("op-1"),
            ops: "stats()".to_string(),
            deadline_ms: Some(5000),
            namespace: Some("research".to_string()),
            actor_id: Some("lambda".to_string()),
            visible_namespaces: Some(vec!["research".to_string(), "ops".to_string()]),
        },
        Frame::Response {
            id: OperationId::from("op-1"),
            result: serde_json::json!({"ok": true, "tool": "stats", "result": {"entities": 3}})
                .into(),
        },
        Frame::Error {
            id: Some(OperationId::from("op-1")),
            code: crate::error::WireErrorCode::PeerClassDenied,
            message: "denied".to_string(),
            unrecognized_code: None,
        },
        Frame::Cancel {
            id: OperationId::from("op-1"),
        },
        Frame::Subscribe {
            id: OperationId::from("op-2"),
            topic: "comm.message_created".to_string(),
            resume_cursor: Some(42),
        },
        Frame::SubscribeAck {
            id: OperationId::from("op-2"),
            topic: "comm.message_created".to_string(),
            start_cursor: 42,
        },
        Frame::Unsubscribe {
            id: OperationId::from("op-3"),
            topic: "comm.message_created".to_string(),
        },
        Frame::UnsubscribeAck {
            id: OperationId::from("op-3"),
            topic: "comm.message_created".to_string(),
        },
        Frame::Event {
            topic: "comm.message_created".to_string(),
            cursor: 43,
            occurred_at: "2026-08-04T11:00:00Z".to_string(),
            payload: serde_json::json!({"message_id": "m-1"}).into(),
        },
    ]
}

include!("codec_tests/framing_tests.rs");

include!("codec_tests/decode_grammar_tests.rs");

include!("codec_tests/encode_validation_tests.rs");

include!("codec_tests/error_compatibility_tests.rs");

include!("codec_tests/buffer_contract_tests.rs");
