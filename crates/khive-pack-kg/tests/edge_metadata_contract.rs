use khive_pack_kg::KgPack;
use khive_runtime::{KhiveRuntime, VerbRegistry, VerbRegistryBuilder};
use serde_json::{json, Value};

fn registry() -> VerbRegistry {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime));
    builder.build().expect("registry")
}

#[tokio::test]
async fn link_metadata_shape_and_optional_are_checked_on_single_and_bulk_paths() {
    let registry = registry();
    let left = registry
        .dispatch("create", json!({"kind": "document", "name": "Left"}))
        .await
        .expect("left document");
    let right = registry
        .dispatch("create", json!({"kind": "document", "name": "Right"}))
        .await
        .expect("right document");
    let source_id = left["id"].as_str().expect("source id");
    let target_id = right["id"].as_str().expect("target id");

    for invalid in [json!(false), json!("text"), json!([])] {
        let error = registry
            .dispatch(
                "link",
                json!({
                    "source_id": source_id,
                    "target_id": target_id,
                    "relation": "depends_on",
                    "metadata": invalid,
                }),
            )
            .await
            .expect_err("non-object metadata must be refused");
        assert!(format!("{error}").contains("metadata must be a JSON object"));
    }

    let bad_optional = json!({
        "source_id": source_id,
        "target_id": target_id,
        "relation": "depends_on",
        "metadata": {"optional": "false"},
    });
    let error = registry
        .dispatch("link", bad_optional.clone())
        .await
        .expect_err("optional string must be refused");
    assert!(format!("{error}").contains("metadata.optional"));

    for atomic in [true, false] {
        let result = registry
            .dispatch(
                "link",
                json!({"links": [{
                    "source_id": source_id,
                    "target_id": target_id,
                    "relation": "depends_on",
                    "metadata": false,
                }], "atomic": atomic}),
            )
            .await;
        if atomic {
            let error = result.expect_err("atomic bulk must reject malformed metadata");
            assert!(format!("{error}").contains("metadata must be a JSON object"));
        } else {
            let receipt = result.expect("best-effort bulk returns per-entry error");
            assert_eq!(receipt["failed"], 1, "{receipt}");
            assert_eq!(receipt["created"], 0, "{receipt}");
        }
    }

    let rejected_edges = registry
        .dispatch("list", json!({"kind": "edge", "source_id": source_id}))
        .await
        .expect("inspect edge set after refusals");
    assert_eq!(rejected_edges["items"].as_array().map(Vec::len), Some(0));

    let empty_metadata = registry
        .dispatch(
            "link",
            json!({
                "source_id": source_id,
                "target_id": target_id,
                "relation": "depends_on",
                "metadata": {},
            }),
        )
        .await
        .expect("object metadata allows dependency inference");
    assert_eq!(empty_metadata["metadata"]["dependency_kind"], "normative");

    let with_optional = registry
        .dispatch(
            "link",
            json!({
                "source_id": source_id,
                "target_id": target_id,
                "relation": "depends_on",
                "metadata": {"optional": false},
            }),
        )
        .await
        .expect("boolean optional is accepted");
    assert_eq!(with_optional["metadata"]["optional"], Value::Bool(false));
    assert_eq!(with_optional["metadata"]["dependency_kind"], "normative");
}
