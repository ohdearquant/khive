fn blob_files(root: &std::path::Path) -> Vec<(std::path::PathBuf, Option<Vec<u8>>)> {
    fn visit(
        root: &std::path::Path,
        path: &std::path::Path,
        files: &mut Vec<(std::path::PathBuf, Option<Vec<u8>>)>,
    ) {
        for entry in std::fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let relative = path.strip_prefix(root).unwrap().to_path_buf();
            if entry.file_type().unwrap().is_dir() {
                files.push((relative, None));
                visit(root, &path, files);
            } else {
                files.push((relative, Some(std::fs::read(path).unwrap())));
            }
        }
    }
    let mut files = Vec::new();
    visit(root, root, &mut files);
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files
}

#[tokio::test]
async fn every_blob_verb_rejects_unknown_arguments_before_side_effects() {
    use serde_json::json;
    let file_policy: khive_runtime::KhiveConfig =
        serde_json::from_value(json!({"blob": {"file_transfers": true}})).unwrap();
    let mut base = khive_runtime::RuntimeConfig::no_embeddings();
    base.db_path = None;
    base.packs = vec!["kg".to_owned()];
    base.brain_profile = None;
    base.actor_id = None;
    let config = khive_runtime::runtime::runtime_config_from_khive_config(&file_policy, base);
    let runtime = KhiveRuntime::new(config).expect("in-memory runtime with file transfers");
    let (registry, _runtime, dir) = build_registry_with_runtime(runtime);
    let existing = registry
        .dispatch("blob.put", json!({"bytes": BASE64.encode(b"existing")}))
        .await
        .unwrap();
    let upload = registry
        .dispatch("blob.begin", json!({"size": 1, "content_ref": null}))
        .await
        .unwrap();
    let id = upload["upload_id"].as_str().unwrap();
    let reference = existing["content_ref"].as_str().unwrap();
    let import_path = dir.path().join("refused-import.bin");
    let export_path = dir.path().join("refused-export.bin");
    std::fs::write(&import_path, b"refused file import").unwrap();
    assert!(std::fs::metadata(&import_path).unwrap().is_file());
    assert!(!export_path.exists());
    // File cases validate argument shape before confinement; transfer success
    // is covered by the file-transfer round trip with isolated roots.
    let cases = [
        (
            "blob.import",
            json!({"path": import_path, "media_type": null}),
        ),
        (
            "blob.export",
            json!({"content_ref": reference, "path": export_path}),
        ),
        (
            "blob.put",
            json!({"bytes": BASE64.encode(b"refused object")}),
        ),
        ("blob.get", json!({"content_ref": reference})),
        ("blob.stat", json!({"content_ref": reference})),
        ("blob.begin", json!({"size": 1})),
        (
            "blob.put_part",
            json!({"upload_id": id, "index": 0, "bytes": BASE64.encode(b"A")}),
        ),
        ("blob.commit", json!({"upload_id": id})),
        ("blob.abort", json!({"upload_id": id})),
    ];
    let registered: std::collections::BTreeSet<_> = BlobPack::HANDLERS
        .iter()
        .map(|handler| handler.name)
        .collect();
    let covered: std::collections::BTreeSet<_> = cases.iter().map(|(verb, _)| *verb).collect();
    assert_eq!(
        covered, registered,
        "every registered verb needs a valid fixture"
    );
    for (verb, params) in [
        ("blob.import", json!([import_path, null])),
        ("blob.export", json!([reference, export_path])),
        ("blob.put", json!([BASE64.encode(b"refused object")])),
        ("blob.get", json!([reference, null])),
        ("blob.stat", json!([reference])),
        ("blob.begin", json!([1, null])),
        ("blob.put_part", json!([id, 0, BASE64.encode(b"A")])),
        ("blob.commit", json!([id])),
        ("blob.abort", json!([id])),
    ] {
        let before = blob_files(dir.path());
        let error = registry
            .dispatch(verb, params)
            .await
            .expect_err("blob verb accepted a positional argument array");
        assert!(
            error
                .to_string()
                .contains("arguments must be a JSON object"),
            "{verb} did not refuse a positional argument array: {error}"
        );
        assert_eq!(
            blob_files(dir.path()),
            before,
            "{verb} mutated state for a positional array"
        );
    }
    let before = blob_files(dir.path());
    for (verb, mut params) in cases {
        params["zzz_not_a_real_param"] = json!(true);
        let outcome = registry.dispatch(verb, params).await;
        assert!(outcome.is_err(), "{verb} accepted an unknown argument");
        let error = outcome.unwrap_err().to_string();
        assert!(
            error.contains("unknown field `zzz_not_a_real_param`"),
            "{verb} did not reject the unknown argument by name: {error}"
        );
        for parameter in BlobPack::HANDLERS
            .iter()
            .find(|handler| handler.name == verb)
            .unwrap()
            .params
        {
            assert!(
                error.contains(parameter.name),
                "{verb} must name allowed argument {}: {error}",
                parameter.name
            );
        }
        assert_eq!(
            blob_files(dir.path()),
            before,
            "{verb} changed blob or staging files"
        );
    }
    // Refused put_part/abort must leave the same upload usable, with no accepted tail.
    let part = registry
        .dispatch(
            "blob.put_part",
            json!({"upload_id": id, "index": 0, "bytes": BASE64.encode(b"B")}),
        )
        .await
        .unwrap();
    assert_eq!(part["received_bytes"], 1);
    assert_eq!(part["next_index"], 1);
    // Exercise a commit that WOULD succeed if the unknown field were ignored.
    let before_commit = blob_files(dir.path());
    let error = registry
        .dispatch(
            "blob.commit",
            json!({"upload_id": id, "zzz_not_a_real_param": true}),
        )
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("unknown field `zzz_not_a_real_param`"),
        "{error}"
    );
    assert_eq!(blob_files(dir.path()), before_commit);
    let committed = registry
        .dispatch("blob.commit", json!({"upload_id": id}))
        .await
        .unwrap();
    let stored = registry
        .dispatch("blob.get", json!({"content_ref": committed["content_ref"]}))
        .await
        .unwrap();
    assert_eq!(
        BASE64.decode(stored["bytes"].as_str().unwrap()).unwrap(),
        b"B"
    );
}

#[tokio::test]
async fn get_range_rejects_unknown_fields_and_preserves_nullable_defaults() {
    use serde_json::json;
    let (registry, _runtime, _dir) = build_registry();
    let stored = registry
        .dispatch("blob.put", json!({"bytes": BASE64.encode(b"abcd")}))
        .await
        .unwrap();
    let reference = stored["content_ref"].as_str().unwrap();
    let error = registry
        .dispatch(
            "blob.get",
            json!({"content_ref": reference, "range": {"offset": 1, "lenght": 2}}),
        )
        .await
        .expect_err("blob.get accepted an unknown range field");
    let error = error.to_string();
    assert!(
        error.contains("unknown field `lenght`")
            && error.contains("offset")
            && error.contains("length"),
        "{error}"
    );
    for range in [serde_json::Value::Null, json!({}), json!({"length": null})] {
        let result = registry
            .dispatch(
                "blob.get",
                json!({"content_ref": reference, "range": range}),
            )
            .await
            .unwrap();
        assert_eq!(
            BASE64.decode(result["bytes"].as_str().unwrap()).unwrap(),
            b"abcd"
        );
    }
    for range in [
        json!([1, 2]),
        json!({"offset": null}),
        json!({"offset": -1}),
        json!({"length": "2"}),
        json!({"length": 1.5}),
    ] {
        assert!(registry
            .dispatch(
                "blob.get",
                json!({"content_ref": reference, "range": range})
            )
            .await
            .is_err());
    }
}
