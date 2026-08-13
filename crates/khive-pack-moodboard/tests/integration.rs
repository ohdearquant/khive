//! Checkpoint-free pack registration and contract tests.

use std::sync::Arc;

use khive_db::stores::blob::FsBlobStore;
use khive_pack_moodboard::MoodboardPack;
use khive_runtime::{
    BackendId, KhiveRuntime, Namespace, PackRegistration, RuntimeConfig, VerbRegistry,
    VerbRegistryBuilder,
};
use khive_storage::{BlobStore, ContentRef};
use khive_types::{EntityKind, Pack};
use sha2::{Digest, Sha256};

fn registry_with_actor(actor_id: Option<&str>) -> VerbRegistry {
    let runtime = KhiveRuntime::memory().expect("memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    if let Some(actor_id) = actor_id {
        builder.with_actor_id(Some(actor_id.to_string()));
    }
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(MoodboardPack::new(runtime));
    builder.build().expect("registry builds")
}

fn registry() -> VerbRegistry {
    registry_with_actor(None)
}

fn core_and_moodboard_runtimes() -> (KhiveRuntime, KhiveRuntime) {
    let make_backend = || {
        let backend = khive_db::StorageBackend::memory().expect("in-memory backend");
        {
            let mut writer = backend.pool().try_writer().expect("writer");
            khive_db::run_migrations(writer.conn_mut()).expect("migrations");
        }
        Arc::new(backend)
    };
    let main_backend = make_backend();
    let secondary_backend = make_backend();
    let mut config = RuntimeConfig::no_embeddings();
    config.packs = vec!["kg".to_string(), "moodboard".to_string()];
    config.backend_id = BackendId::new("moodboard");
    let moodboard =
        KhiveRuntime::from_backend(secondary_backend, config).with_core_backend(main_backend);
    let core = moodboard.core();
    (core, moodboard)
}

fn preference_pair_split(
    board_id: &str,
    descriptor_fingerprint: &str,
    feature_schema_id: &str,
    left_ref: &str,
    right_ref: &str,
) -> &'static str {
    let (lower, upper) = if left_ref <= right_ref {
        (left_ref, right_ref)
    } else {
        (right_ref, left_ref)
    };
    let mut hasher = Sha256::new();
    hasher.update(b"moodboard-pair-split-v1\0");
    for field in [
        board_id,
        descriptor_fingerprint,
        feature_schema_id,
        lower,
        upper,
    ] {
        hasher.update(field.as_bytes());
        hasher.update([0]);
    }
    let digest = hasher.finalize();
    match u64::from_be_bytes(digest[..8].try_into().unwrap()) % 20 {
        0..=13 => "train",
        14..=16 => "calibration",
        _ => "test",
    }
}

#[allow(clippy::too_many_arguments)]
async fn record_preference_pair(
    registry: &VerbRegistry,
    board_entity_id: uuid::Uuid,
    board_id: &str,
    descriptor_fingerprint: &str,
    anchor_id: uuid::Uuid,
    anchor_ref: &ContentRef,
    other_id: uuid::Uuid,
    other_ref: &ContentRef,
    choice: &str,
    reason_code: &str,
) -> serde_json::Value {
    let serve = registry
        .dispatch(
            "moodboard.serve",
            serde_json::json!({
                "board_entity_id": board_entity_id,
                "board_id": board_id,
                "descriptor": {
                    "model_key": "fixture_visual_model",
                    "descriptor_fingerprint": descriptor_fingerprint,
                },
                "source_report_sha256": "c".repeat(64),
                "candidates": [
                    {
                        "state": "scored",
                        "asset_id": anchor_id,
                        "content_ref": anchor_ref,
                        "features": [0.9, 0.8, 0.7, 0.6, 0.5, 0.4, 0.3, 0.2, 0.1, 0.0],
                    },
                    {
                        "state": "scored",
                        "asset_id": other_id,
                        "content_ref": other_ref,
                        "features": [0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9, 1.0],
                    }
                ],
                "selection": {
                    "policy_revision": "support-fixture-v1",
                    "pair_propensity": 0.5,
                    "candidate_pool_sha256": "d".repeat(64),
                }
            }),
        )
        .await
        .expect("serve pair");
    registry
        .dispatch(
            "moodboard.judge",
            serde_json::json!({
                "serve_id": serve["serve_id"],
                "left_result_occurrence_id": serve["left"]["result_occurrence_id"],
                "right_result_occurrence_id": serve["right"]["result_occurrence_id"],
                "choice": choice,
                "reason_code": reason_code,
            }),
        )
        .await
        .expect("judge pair")
}

#[test]
fn pack_identity_dependencies_and_inventory_are_stable() {
    assert_eq!(MoodboardPack::NAME, "moodboard");
    assert_eq!(MoodboardPack::REQUIRES, &["kg"]);
    assert!(MoodboardPack::NOTE_KINDS.is_empty());
    assert!(MoodboardPack::ENTITY_KINDS.is_empty());
    assert!(inventory::iter::<PackRegistration>
        .into_iter()
        .any(|registration| registration.0.name() == "moodboard"));
}

#[test]
fn pack_contributes_only_additive_artifact_subtypes() {
    let types = MoodboardPack::ENTITY_TYPES;
    assert_eq!(types.len(), 3);
    assert!(types
        .iter()
        .all(|definition| definition.kind == EntityKind::Artifact));
    assert_eq!(types[0].type_name, "visual_asset");
    assert_eq!(types[1].type_name, "moodboard");
    assert_eq!(types[2].type_name, "moodboard_model");
}

#[tokio::test]
async fn registry_exposes_exact_v1_handler_names() {
    let registry = registry();
    let expected = [
        "moodboard.model",
        "moodboard.ingest",
        "moodboard.search",
        "moodboard.serve",
        "moodboard.judge",
        "moodboard.train_preference",
        "moodboard.preference",
    ];
    assert_eq!(
        MoodboardPack::HANDLERS
            .iter()
            .map(|handler| handler.name)
            .collect::<Vec<_>>(),
        expected
    );
    let serve = MoodboardPack::HANDLERS
        .iter()
        .find(|handler| handler.name == "moodboard.serve")
        .expect("moodboard.serve handler metadata");
    let serve_params = serve
        .params
        .iter()
        .map(|parameter| parameter.name)
        .collect::<Vec<_>>();
    assert!(serve_params.contains(&"exposure"));
    assert!(!serve_params.contains(&"presentation"));
    for verb in expected {
        let help = registry
            .dispatch(verb, serde_json::json!({"help": true}))
            .await
            .expect("help dispatch");
        assert!(help["params"].is_array());
    }
}

#[tokio::test]
async fn preference_verbs_reject_every_unattributed_actor_before_payloads() {
    for (identity, registry) in [
        ("anonymous fallback", registry()),
        ("explicit local", registry_with_actor(Some("local"))),
    ] {
        for verb in [
            "moodboard.serve",
            "moodboard.judge",
            "moodboard.train_preference",
            "moodboard.preference",
        ] {
            let error = registry
                .dispatch(verb, serde_json::json!({}))
                .await
                .expect_err("unattributed preference verb must fail");
            assert!(
                error.to_string().contains("explicitly configured actor"),
                "{identity} {verb} returned unexpected error: {error}"
            );
        }
    }
}

#[tokio::test]
async fn ingest_rejects_missing_bytes_before_touching_optional_substrates() {
    let error = registry()
        .dispatch("moodboard.ingest", serde_json::json!({}))
        .await
        .expect_err("missing base64 must fail");
    assert!(error.to_string().contains("image_base64"));
}

#[tokio::test]
async fn search_requires_a_bare_canonical_uuid() {
    let error = registry()
        .dispatch(
            "moodboard.search",
            serde_json::json!({"asset_id": "entity:not-a-uuid"}),
        )
        .await
        .expect_err("namespaced identifier must fail");
    assert!(error.to_string().contains("UUID"));
}

#[tokio::test]
async fn attributed_serve_randomizes_occurrences_and_judgment_is_immutable() {
    let runtime = KhiveRuntime::memory().expect("memory runtime");
    let root = tempfile::tempdir().expect("blob root");
    let blob_store = Arc::new(FsBlobStore::new(root.path().to_path_buf(), 0).expect("blob store"));
    runtime.install_blob_store(blob_store.clone());
    let setup_token = runtime
        .authorize(Namespace::parse("moodboard-source").expect("source namespace"))
        .expect("setup token");
    let board_fingerprint = "a".repeat(64);
    let board = runtime
        .create_entity(
            &setup_token,
            "artifact",
            Some("moodboard"),
            "preference test board",
            None,
            Some(serde_json::json!({"board_id": board_fingerprint})),
            vec![],
        )
        .await
        .expect("board");
    let mut assets = Vec::new();
    for (index, bytes) in [b"asset-one".as_slice(), b"asset-two".as_slice()]
        .into_iter()
        .enumerate()
    {
        let content_ref = blob_store.put(bytes.to_vec()).await.expect("asset blob");
        let asset = runtime
            .create_entity_with_content_ref(
                &setup_token,
                "artifact",
                Some("visual_asset"),
                &format!("asset-{index}"),
                None,
                Some(serde_json::json!({"schema_version": "fixture"})),
                vec![],
                &content_ref,
            )
            .await
            .expect("asset");
        assets.push((asset, content_ref));
    }

    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some("moodboard-tester".to_string()));
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(MoodboardPack::new(runtime.clone()));
    let registry = builder.build().expect("actor registry");
    let serve_payload = serde_json::json!({
        "board_entity_id": board.id,
        "board_id": board_fingerprint,
        "descriptor": {
            "model_key": "fixture_visual_model",
            "descriptor_fingerprint": "b".repeat(64),
        },
        "source_report_sha256": "c".repeat(64),
        "candidates": [
            {
                "state": "scored",
                "asset_id": assets[0].0.id,
                "content_ref": assets[0].1,
                "source_rank": 1,
                "features": [0.9, 0.8, 0.7, 0.6, 0.5, 0.4, 0.3, 0.2, 0.1, 0.0],
            },
            {
                "state": "scored",
                "asset_id": assets[1].0.id,
                "content_ref": assets[1].1,
                "source_rank": 2,
                "features": [0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9, 1.0],
            }
        ],
        "selection": {
            "policy_revision": "uniform-pair-v1",
            "pair_propensity": 0.5,
            "candidate_pool_sha256": "d".repeat(64),
        },
        "exposure": {
            "preference_probability_shown": false,
            "source_rank_shown": true,
        }
    });
    let serve = registry
        .dispatch("moodboard.serve", serve_payload.clone())
        .await
        .expect("serve");
    let serve_id = serve["serve_id"].as_str().expect("serve id");
    let serve_uuid = uuid::Uuid::parse_str(serve_id).expect("serve uuid");
    let event_token = runtime
        .authorize(Namespace::local())
        .expect("event namespace token");
    let event = runtime
        .events(&event_token)
        .expect("events")
        .get_event(serve_uuid)
        .await
        .expect("event read")
        .expect("serve event");
    assert_eq!(event.verb, "moodboard.serve_record");
    assert_eq!(event.actor, "actor:moodboard-tester");
    assert_eq!(event.payload["serve_id"], serve["serve_id"]);
    assert!(matches!(
        event.payload["left"]["source_candidate_index"].as_u64(),
        Some(0 | 1)
    ));
    assert!(event.payload["randomization"]["swap_applied"].is_boolean());
    assert_eq!(event.payload["presentation"]["source_rank_shown"], true);
    assert_eq!(
        event.payload["presentation"]["preference_probability_shown"],
        false
    );
    assert_eq!(
        event.payload["presentation"]["served_preference_model_id"],
        serde_json::Value::Null
    );
    assert!(
        event.payload.get("exposure").is_none(),
        "durable v1 payload identity must retain its presentation member"
    );
    assert_ne!(
        serve["left"]["result_occurrence_id"],
        serve["right"]["result_occurrence_id"]
    );

    let mut missing_model_payload = serve_payload.clone();
    missing_model_payload["exposure"] = serde_json::json!({
        "preference_probability_shown": true,
        "source_rank_shown": true,
    });
    let missing_model_error = registry
        .dispatch("moodboard.serve", missing_model_payload)
        .await
        .expect_err("probability exposure requires its governed model identity");
    assert!(
        missing_model_error
            .to_string()
            .contains("exposure.served_preference_model_id is required"),
        "unexpected error: {missing_model_error}"
    );

    let mut unshown_model_payload = serve_payload.clone();
    unshown_model_payload["exposure"] = serde_json::json!({
        "preference_probability_shown": false,
        "source_rank_shown": true,
        "served_preference_model_id": "00000000-0000-4000-8000-000000000301",
    });
    let unshown_model_error = registry
        .dispatch("moodboard.serve", unshown_model_payload)
        .await
        .expect_err("an unshown probability cannot claim a served model");
    assert!(
        unshown_model_error
            .to_string()
            .contains("exposure.served_preference_model_id is only valid"),
        "unexpected error: {unshown_model_error}"
    );

    let mut default_payload = serve_payload;
    default_payload
        .as_object_mut()
        .expect("serve payload object")
        .remove("exposure");
    let default_serve = registry
        .dispatch("moodboard.serve", default_payload)
        .await
        .expect("default exposure serve");
    let default_serve_id = uuid::Uuid::parse_str(
        default_serve["serve_id"]
            .as_str()
            .expect("default serve id"),
    )
    .expect("default serve uuid");
    let default_event = runtime
        .events(&event_token)
        .expect("events")
        .get_event(default_serve_id)
        .await
        .expect("default event read")
        .expect("default serve event");
    assert_eq!(
        default_event.payload["presentation"],
        serde_json::json!({
            "preference_probability_shown": false,
            "source_rank_shown": false,
            "served_preference_model_id": null,
        })
    );

    let judgment_payload = serde_json::json!({
        "serve_id": serve_id,
        "left_result_occurrence_id": serve["left"]["result_occurrence_id"],
        "right_result_occurrence_id": serve["right"]["result_occurrence_id"],
        "choice": "tie",
        "reason_code": "equally_good",
        "response_ms": 250,
    });
    let first = registry
        .dispatch("moodboard.judge", judgment_payload.clone())
        .await
        .expect("first judgment");
    assert_eq!(first["created"], true);
    let retry = registry
        .dispatch("moodboard.judge", judgment_payload)
        .await
        .expect("exact retry");
    assert_eq!(retry["created"], false);

    let conflict = registry
        .dispatch(
            "moodboard.judge",
            serde_json::json!({
                "serve_id": serve_id,
                "left_result_occurrence_id": serve["left"]["result_occurrence_id"],
                "right_result_occurrence_id": serve["right"]["result_occurrence_id"],
                "choice": "right",
                "reason_code": "style",
            }),
        )
        .await
        .expect_err("conflicting judgment must fail");
    assert!(conflict.to_string().contains("conflicting immutable"));
}

#[tokio::test]
async fn public_training_publishes_calibrated_fann_and_preference_stays_nonconformal() {
    const FEATURE_SCHEMA_ID: &str =
        "f691fc73bf9a50d72157e21601fa579caa707bf2c448df546c63e915b4e42175";
    let (runtime, moodboard_runtime) = core_and_moodboard_runtimes();
    let root = tempfile::tempdir().expect("blob root");
    let blob_store = Arc::new(FsBlobStore::new(root.path().to_path_buf(), 0).expect("blob store"));
    moodboard_runtime.install_blob_store(blob_store.clone());
    let setup_token = runtime.authorize(Namespace::local()).expect("setup token");
    let board_fingerprint = "a".repeat(64);
    let descriptor_fingerprint = "b".repeat(64);
    let board = runtime
        .create_entity(
            &setup_token,
            "artifact",
            Some("moodboard"),
            "training acceptance board",
            None,
            Some(serde_json::json!({"board_id": board_fingerprint})),
            vec![],
        )
        .await
        .expect("board");
    let anchor_ref = blob_store
        .put(b"anchor".to_vec())
        .await
        .expect("anchor blob");
    let anchor = runtime
        .create_entity_with_content_ref(
            &setup_token,
            "artifact",
            Some("visual_asset"),
            "anchor",
            None,
            Some(serde_json::json!({"schema_version": "fixture"})),
            vec![],
            &anchor_ref,
        )
        .await
        .expect("anchor asset");

    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some("preference-trainer".to_string()));
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(MoodboardPack::new(moodboard_runtime.clone()));
    let registry = builder.build().expect("actor registry");

    let targets = std::collections::BTreeMap::from([
        ("train", 64usize),
        ("calibration", 16usize),
        ("test", 16usize),
    ]);
    let mut counts = std::collections::BTreeMap::from([
        ("train", 0usize),
        ("calibration", 0usize),
        ("test", 0usize),
    ]);
    let mut last_other = None;
    for candidate_index in 0u64..100_000 {
        if counts.iter().all(|(split, count)| *count >= targets[split]) {
            break;
        }
        let bytes = format!("candidate-{candidate_index}").into_bytes();
        let candidate_ref = ContentRef::from_digest_bytes(blake3::hash(&bytes).as_bytes());
        let split = preference_pair_split(
            &board_fingerprint,
            &descriptor_fingerprint,
            FEATURE_SCHEMA_ID,
            anchor_ref.as_str(),
            candidate_ref.as_str(),
        );
        let sequence = counts[split];
        if sequence >= targets[split] {
            continue;
        }
        let stored_ref = blob_store.put(bytes).await.expect("candidate blob");
        assert_eq!(stored_ref, candidate_ref);
        let other = runtime
            .create_entity_with_content_ref(
                &setup_token,
                "artifact",
                Some("visual_asset"),
                &format!("candidate-{candidate_index}"),
                None,
                Some(serde_json::json!({"schema_version": "fixture"})),
                vec![],
                &candidate_ref,
            )
            .await
            .expect("candidate asset");
        let (choice, reason) = if sequence % 2 == 0 {
            ("left", "style")
        } else {
            ("right", "style")
        };
        record_preference_pair(
            &registry,
            board.id,
            &board_fingerprint,
            &descriptor_fingerprint,
            anchor.id,
            &anchor_ref,
            other.id,
            &candidate_ref,
            choice,
            reason,
        )
        .await;
        if split == "calibration" {
            record_preference_pair(
                &registry,
                board.id,
                &board_fingerprint,
                &descriptor_fingerprint,
                anchor.id,
                &anchor_ref,
                other.id,
                &candidate_ref,
                "tie",
                "equally_good",
            )
            .await;
        }
        counts.insert(split, sequence + 1);
        last_other = Some((other, candidate_ref));
    }
    assert_eq!(counts, targets, "fixture must meet exact production gates");

    let trained = registry
        .dispatch(
            "moodboard.train_preference",
            serde_json::json!({
                "board_entity_id": board.id,
                "board_id": board_fingerprint,
                "descriptor": {
                    "model_key": "fixture_visual_model",
                    "descriptor_fingerprint": descriptor_fingerprint,
                },
                "feature_schema_id": FEATURE_SCHEMA_ID,
            }),
        )
        .await
        .expect("public training");
    assert_eq!(trained["fann_inference_verified"], true);
    assert_eq!(trained["calibration"]["calibrated"], true);
    assert_eq!(
        trained["training"]["split_counts"]["train"]["decisive_groups"],
        64
    );
    assert_eq!(
        trained["training"]["split_counts"]["calibration"]["tie_groups"],
        16
    );
    let model_id =
        uuid::Uuid::parse_str(trained["preference_model_id"].as_str().expect("model id"))
            .expect("model UUID");
    assert_eq!(
        runtime
            .get_entity(&setup_token, model_id)
            .await
            .expect("model must live in core")
            .entity_type
            .as_deref(),
        Some("moodboard_model")
    );
    assert!(
        moodboard_runtime
            .get_entity(&setup_token, model_id)
            .await
            .is_err(),
        "preference model graph identity must not leak into the pack-selected backend"
    );
    assert!(
        moodboard_runtime
            .get_entity(&setup_token, anchor.id)
            .await
            .is_err(),
        "visual asset graph identity must remain in core"
    );
    let (other, other_ref) = last_other.expect("at least one other asset");
    let preference = registry
        .dispatch(
            "moodboard.preference",
            serde_json::json!({
                "preference_model_id": trained["preference_model_id"],
                "board_entity_id": board.id,
                "board_id": board_fingerprint,
                "descriptor": {
                    "model_key": "fixture_visual_model",
                    "descriptor_fingerprint": descriptor_fingerprint,
                },
                "feature_schema_id": FEATURE_SCHEMA_ID,
                "source_report_sha256": "c".repeat(64),
                "left": {
                    "state": "scored",
                    "asset_id": anchor.id,
                    "content_ref": anchor_ref,
                    "features": [0.9, 0.8, 0.7, 0.6, 0.5, 0.4, 0.3, 0.2, 0.1, 0.0],
                },
                "right": {
                    "state": "scored",
                    "asset_id": other.id,
                    "content_ref": other_ref,
                    "features": [0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9, 1.0],
                },
            }),
        )
        .await
        .expect("preference inference");
    assert_eq!(preference["prediction_kind"], "learned_pairwise_preference");
    assert_eq!(
        preference["conformal_evidence"]["state"],
        "not_computed_by_this_verb"
    );
    assert!(preference.get("style_conformal_p").is_none());
    let left_probability = preference["probability_left_given_decisive"]
        .as_f64()
        .expect("left probability");
    let right_probability = preference["probability_right_given_decisive"]
        .as_f64()
        .expect("right probability");
    assert!((left_probability + right_probability - 1.0).abs() <= f64::EPSILON);
}

#[tokio::test]
async fn serve_rejects_source_rank_shown_without_candidate_ranks() {
    // Pure input validation: the pairing rule fires before any board or asset
    // lookup, so no fixture entities exist behind this registry on purpose —
    // reaching a "board not found" error instead would mean the check ran too
    // late to protect the immutable record cheaply.
    let registry = registry_with_actor(Some("moodboard-tester"));
    let error = registry
        .dispatch(
            "moodboard.serve",
            serde_json::json!({
                "board_entity_id": "00000000-0000-4000-8000-000000000201",
                "board_id": "a".repeat(64),
                "descriptor": {
                    "model_key": "fixture_visual_model",
                    "descriptor_fingerprint": "b".repeat(64),
                },
                "source_report_sha256": "c".repeat(64),
                "candidates": [
                    {
                        "state": "scored",
                        "asset_id": "00000000-0000-4000-8000-000000000101",
                        "content_ref": "1".repeat(64),
                        "features": [0.9, 0.8, 0.7, 0.6, 0.5, 0.4, 0.3, 0.2, 0.1, 0.0],
                    },
                    {
                        "state": "scored",
                        "asset_id": "00000000-0000-4000-8000-000000000102",
                        "content_ref": "2".repeat(64),
                        "source_rank": 2,
                        "features": [0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9, 1.0],
                    }
                ],
                "selection": {
                    "policy_revision": "support-fixture-v1",
                },
                "exposure": {
                    "source_rank_shown": true,
                }
            }),
        )
        .await
        .expect_err("source_rank_shown without both ranks must be refused");
    assert!(
        error.to_string().contains("requires source_rank"),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn serve_rejects_reserved_presentation_as_a_business_argument() {
    let error = registry_with_actor(Some("moodboard-tester"))
        .dispatch(
            "moodboard.serve",
            serde_json::json!({
                "presentation": {
                    "source_rank_shown": true,
                }
            }),
        )
        .await
        .expect_err("the envelope-reserved presentation spelling must not be a serve argument");
    assert!(
        error.to_string().contains("unknown field `presentation`"),
        "unexpected error: {error}"
    );
}
