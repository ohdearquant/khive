#[path = "support/const_vec.rs"]
mod const_vec;
#[path = "../../khive-runtime/tests/support/receipt_credentials.rs"]
mod receipt_credentials;

use const_vec::ConstVecProvider;
use khive_pack_kg::KgPack;
use khive_pack_memory::MemoryPack;
use khive_runtime::{KhiveRuntime, RuntimeConfig, VerbRegistry, VerbRegistryBuilder};
use serde_json::json;

fn make_runtime() -> KhiveRuntime {
    KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        embedding_model: None,
        additional_embedding_models: vec![],
        ..RuntimeConfig::default()
    })
    .expect("in-memory runtime")
}

fn make_registry(rt: KhiveRuntime) -> VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(MemoryPack::new(
        receipt_credentials::with_receipt_credentials(rt),
    ));
    builder.build().expect("registry builds")
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn sealed_receipts_hide_intervening_namespace_writes_and_keep_own_session_visibility() {
    const MODEL: &str = "all-minilm-l6-v2";
    const OWN: &str = "receipt-owner-a";
    const OTHER: &str = "receipt-other-b";
    const FOREIGN_WRITES: u64 = 10;
    const LATEST: &str = "quartz heron cobalt lighthouse latest owner receipt";

    let rt = receipt_credentials::with_receipt_credentials(make_runtime());
    rt.register_embedder(ConstVecProvider::new(
        MODEL,
        lattice_embed::EmbeddingModel::AllMiniLmL6V2.dimensions(),
        0.9,
    ));
    let registry = make_registry(rt.clone());
    let first = registry
        .dispatch(
            "memory.remember",
            json!({
                "content": "quartz heron first owner receipt", "memory_type": "semantic",
                "namespace": OWN, "embedding_model": MODEL, "salience": 0.5,
            }),
        )
        .await
        .expect("first owner write");
    let first_token = first["visibility_token"]
        .as_str()
        .expect("opaque first receipt");
    let first_receipt = rt
        .open_visibility_receipt(first_token, &[OWN], &[MODEL.to_owned()])
        .expect("authenticate first owner's actual fence");
    assert_eq!(first_receipt.namespace(), OWN);
    let first_seq = first_receipt
        .sequence_for_model(MODEL)
        .expect("first vector fence");
    assert!(
        (1..10).contains(&first_seq),
        "fixture starts below a decimal-width boundary"
    );

    let mut preceding_seq = first_seq;
    let mut foreign_ids = Vec::new();
    for i in 0..FOREIGN_WRITES {
        let foreign = registry
            .dispatch(
                "memory.remember",
                json!({
                    "content": format!("foreign intervening archive receipt {i}"),
                    "memory_type": "semantic", "namespace": OTHER,
                    "embedding_model": MODEL, "salience": 0.5,
                }),
            )
            .await
            .expect("foreign intervening write");
        let receipt = rt
            .open_visibility_receipt(
                foreign["visibility_token"]
                    .as_str()
                    .expect("opaque foreign receipt"),
                &[OTHER],
                &[MODEL.to_owned()],
            )
            .expect("authenticate actual foreign write for the fixture oracle");
        assert_eq!(receipt.namespace(), OTHER);
        let sequence = receipt
            .sequence_for_model(MODEL)
            .expect("foreign vector fence");
        assert_eq!(sequence, preceding_seq + 1);
        preceding_seq = sequence;
        foreign_ids.push(foreign["id"].as_str().expect("foreign ID").to_owned());
    }

    let latest = registry
        .dispatch(
            "memory.remember",
            json!({
                "content": LATEST, "memory_type": "semantic", "namespace": OWN,
                "embedding_model": MODEL, "salience": 1.0,
            }),
        )
        .await
        .expect("latest owner write after foreign progress");
    let latest_token = latest["visibility_token"]
        .as_str()
        .expect("opaque latest receipt");
    let latest_receipt = rt
        .open_visibility_receipt(latest_token, &[OWN], &[MODEL.to_owned()])
        .expect("authenticate latest owner's actual fence");
    assert_eq!(latest_receipt.namespace(), OWN);
    let latest_seq = latest_receipt
        .sequence_for_model(MODEL)
        .expect("latest vector fence");
    assert_eq!(latest_seq, preceding_seq + 1);
    assert_eq!(latest_seq - first_seq, FOREIGN_WRITES + 1);
    assert!(
        latest_seq >= 10,
        "real writes cross the decimal-width boundary"
    );
    assert_eq!(
        first_token.len(),
        latest_token.len(),
        "fixed-width encrypted sequences must not reveal the larger global counter"
    );
    assert_ne!(first_token, latest_token);

    for response in [&first, &latest] {
        let token = response["visibility_token"]
            .as_str()
            .expect("opaque public string");
        assert!(token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')));
        let public = response.to_string();
        for field in [
            "namespace",
            "model",
            "fences",
            "seq",
            "ann_write_log_seq",
            "issued_at",
        ] {
            assert!(
                !public.contains(&format!("\"{field}\":")),
                "public receipt exposed {field}"
            );
        }
        for sentinel in [OWN, OTHER, MODEL] {
            assert!(
                !public.contains(sentinel),
                "public receipt exposed {sentinel}"
            );
        }
    }

    let recalled = registry
        .dispatch(
            "memory.recall",
            json!({
                "query": LATEST, "namespace": OWN, "embedding_model": MODEL,
                "consistency": "session", "visibility_token": latest_token,
                "fusion_strategy": "vector_only", "timeout_ms": 0,
                "limit": 20, "score_floor": 0.0,
            }),
        )
        .await
        .expect("latest owner's actual fence proves its candidate read");
    let hits = recalled.as_array().expect("recall results");
    assert!(
        hits.iter().any(|hit| hit["id"] == latest["id"]),
        "session recall lost the latest owner memory: {recalled:?}"
    );
    for id in foreign_ids {
        assert!(
            !hits
                .iter()
                .any(|hit| hit["id"].as_str() == Some(id.as_str())),
            "own session recall included a foreign memory"
        );
    }
}
