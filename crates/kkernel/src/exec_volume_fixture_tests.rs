#[tokio::test]
async fn ops_file_applies_ops_and_summary_matches() {
    if crate::test_process::run_in_child() {
        return;
    }

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();
    let server = isolated_server(&db_path);

    // Write 3 create-entity ops.
    let mut f = NamedTempFile::new().unwrap();
    use std::io::Write as _;
    for name in ["Alpha", "Beta", "Gamma"] {
        let line = format!(
            "{{\"tool\":\"create\",\"args\":{{\"kind\":\"concept\",\"name\":\"{name}\"}}}}\n"
        );
        f.write_all(line.as_bytes()).unwrap();
    }

    let ops = parse_ops_file(f.path()).unwrap();
    assert_eq!(ops.len(), 3);
    let summary = apply_ops_file(&server, ops, None, None, None, false)
        .await
        .unwrap();
    assert_eq!(summary["total"], 3);
    assert_eq!(summary["succeeded"], 3);
    assert_eq!(summary["failed"], 0);
    assert!(summary.get("aborted").is_none());
    assert!(summary.get("failure_details_omitted").is_none());
    assert!(summary.get("results").is_none());

    // Verify all 3 entities are present.
    let params = RequestParams {
        plan: None,
        ops: r#"list(kind="concept")"#.to_string(),
        presentation: None,
        presentation_per_op: None,
        save_to: None,
        format: None,
        format_per_op: None,
        request_id: None,
    };
    let raw = server.dispatch_request_local(params).await.unwrap();
    let resp: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let count = resp["results"][0]["result"]["items"]
        .as_array()
        .map(|a| a.len())
        .unwrap_or(0);
    assert_eq!(
        count, 3,
        "all 3 entities should be present after apply\nraw: {resp}"
    );
}

#[tokio::test]
async fn oversized_single_ops_file_reaches_handler_validation() {
    if crate::test_process::run_in_child() {
        return;
    }

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();
    let server = isolated_server(&db_path);
    let ops = vec![OpsFileEntry {
        tool: "stats".to_string(),
        args: serde_json::json!({
            "payload": "x".repeat(khive_request::MAX_OPS_INPUT_LEN + 1),
        }),
    }];
    let mut observed_handler_error = false;

    let error = apply_ops_file_with_response_transform(
        &server,
        ops,
        Some("verbose".to_string()),
        Some("json".to_string()),
        None,
        false,
        |_, raw| {
            let response: serde_json::Value =
                serde_json::from_str(&raw).expect("handler response must be JSON");
            assert_eq!(response["results"][0]["tool"], "stats");
            assert_eq!(response["results"][0]["ok"], false);
            assert!(
                response["results"][0]["error"]
                    .to_string()
                    .contains("payload"),
                "the oversized typed op must reach stats argument validation: {response}"
            );
            observed_handler_error = true;
            raw
        },
    )
    .await
    .expect_err("the only op is intentionally invalid at the handler boundary");

    assert!(
        observed_handler_error,
        "ops-file dispatch must not reapply the public 1 MiB raw-DSL limit"
    );
    assert!(error.to_string().contains("every op failed"));
}

#[tokio::test]
async fn oversized_multi_op_chunk_preserves_order_save_and_strict() {
    if crate::test_process::run_in_child() {
        return;
    }

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();
    let server = isolated_server(&db_path);
    let payload = "x".repeat(600 * 1024);
    let ops = vec![
        OpsFileEntry {
            tool: "create".to_string(),
            args: serde_json::json!({
                "kind": "concept",
                "name": "oversized ordered success",
                "description": payload,
            }),
        },
        OpsFileEntry {
            tool: "stats".to_string(),
            args: serde_json::json!({
                "payload": "y".repeat(600 * 1024),
            }),
        },
    ];
    let encoded_len = serde_json::to_vec(&ops).unwrap().len();
    assert!(encoded_len > khive_request::MAX_OPS_INPUT_LEN);
    let output_dir = tempfile::tempdir().unwrap();
    let save_path = output_dir.path().join("oversized-ordered.jsonl");

    let error = apply_ops_file(
        &server,
        ops,
        Some("verbose".to_string()),
        Some("json".to_string()),
        Some(save_path.to_string_lossy().into_owned()),
        true,
    )
    .await
    .expect_err("strict mode must report the handler-level stats failure");

    assert!(error.to_string().contains("--strict"), "{error:#}");
    let rows: Vec<serde_json::Value> = std::fs::read_to_string(&save_path)
        .expect("strict failure still publishes the complete ordered result file")
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["tool"], "create");
    assert_eq!(rows[0]["ok"], true);
    assert_eq!(rows[1]["tool"], "stats");
    assert_eq!(rows[1]["ok"], false);
    assert_eq!(rows[1]["reason"], "strict-op-failure");
}

#[tokio::test]
async fn public_dispatch_still_rejects_oversized_raw_ops() {
    if crate::test_process::run_in_child() {
        return;
    }

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();
    let server = isolated_server(&db_path);
    let params = RequestParams {
        plan: None,
        ops: serde_json::json!({
            "tool": "stats",
            "args": {"payload": "x".repeat(khive_request::MAX_OPS_INPUT_LEN + 1)},
        })
        .to_string(),
        presentation: Some("verbose".to_string()),
        presentation_per_op: None,
        save_to: None,
        format: Some("json".to_string()),
        format_per_op: None,
        request_id: None,
    };

    let error = server
        .dispatch_request_local(params)
        .await
        .expect_err("the raw request surface must retain its 1 MiB safety bound");
    assert!(
        error.to_string().contains("ops input is")
            && error.to_string().contains(&format!(
                "max is {} bytes",
                khive_request::MAX_OPS_INPUT_LEN
            )),
        "unexpected public dispatch error: {error}"
    );
}

#[tokio::test]
async fn multi_chunk_save_retains_order_rows_checksum_and_json_override() {
    if crate::test_process::run_in_child() {
        return;
    }

    use sha2::Digest as _;

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();
    let server =
        isolated_server(&db_path).with_default_output_format(khive_runtime::OutputFormat::Table);
    let ops: Vec<OpsFileEntry> = (0..=OPS_FILE_CHUNK_SIZE)
        .map(|index| OpsFileEntry {
            tool: "create".to_string(),
            args: serde_json::json!({
                "kind": "concept",
                "name": format!("ordered-{index:03}"),
            }),
        })
        .collect();
    let output_dir = tempfile::tempdir().unwrap();
    let save_path = output_dir.path().join("ordered.jsonl");

    let manifest = apply_ops_file(
        &server,
        ops,
        Some("verbose".to_string()),
        Some("json".to_string()),
        Some(save_path.to_string_lossy().into_owned()),
        true,
    )
    .await
    .unwrap();

    let manifest_keys: std::collections::BTreeSet<_> = manifest
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        manifest_keys,
        std::collections::BTreeSet::from([
            "checksum",
            "path",
            "per_column_null_counts",
            "rows",
            "schema_fingerprint",
            "summary",
        ]),
        "the successful manifest shape must remain unchanged"
    );
    assert_eq!(manifest["rows"], OPS_FILE_CHUNK_SIZE + 1);
    assert_eq!(manifest["summary"]["total"], OPS_FILE_CHUNK_SIZE + 1);
    assert_eq!(manifest["summary"]["succeeded"], OPS_FILE_CHUNK_SIZE + 1);
    assert_eq!(manifest["summary"]["failed"], 0);
    assert_eq!(manifest["summary"]["aborted"], 0);

    let bytes = std::fs::read(&save_path).unwrap();
    let checksum = format!("{:x}", sha2::Sha256::digest(&bytes));
    assert_eq!(manifest["checksum"], checksum);
    let rows: Vec<serde_json::Value> = bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    assert_eq!(rows.len(), OPS_FILE_CHUNK_SIZE + 1);
    for (index, row) in rows.iter().enumerate() {
        assert_eq!(row["tool"], "create");
        assert_eq!(row["ok"], true);
        assert_eq!(row["result"]["name"], format!("ordered-{index:03}"));
    }
}

#[tokio::test]
async fn malformed_later_chunk_emits_aborted_manifest_for_prior_commits() {
    if crate::test_process::run_in_child() {
        return;
    }

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();
    let server = isolated_server(&db_path);
    let ops: Vec<OpsFileEntry> = (0..=OPS_FILE_CHUNK_SIZE)
        .map(|index| OpsFileEntry {
            tool: "create".to_string(),
            args: serde_json::json!({
                "kind": "concept",
                "name": format!("abort-manifest-{index:03}"),
            }),
        })
        .collect();
    let output_dir = tempfile::tempdir().unwrap();
    let save_path = output_dir.path().join("must-not-publish.jsonl");

    let error = apply_ops_file_with_response_transform(
        &server,
        ops,
        Some("verbose".to_string()),
        Some("json".to_string()),
        Some(save_path.to_string_lossy().into_owned()),
        true,
        |chunk_number, raw| {
            if chunk_number == 2 {
                "{malformed-response".to_string()
            } else {
                raw
            }
        },
    )
    .await
    .unwrap_err();

    let aborted = error
        .downcast_ref::<AbortedOpsFileError>()
        .expect("post-dispatch failure must carry its emitted manifest");
    assert_eq!(aborted.manifest["status"], "aborted");
    assert_eq!(aborted.manifest["committed_chunks"], serde_json::json!([1]));
    assert_eq!(aborted.manifest["dispatched_chunk"], 2);
    assert_eq!(aborted.manifest["file_published"], false);
    assert_eq!(
        aborted.manifest["summary"]["succeeded"],
        OPS_FILE_CHUNK_SIZE
    );
    assert_eq!(aborted.manifest["summary"]["total"], OPS_FILE_CHUNK_SIZE);
    assert_eq!(aborted.manifest["summary"]["aborted"], 0);
    assert_eq!(aborted.manifest["unconfirmed_ops"], 1);
    assert!(
        !save_path.exists(),
        "an aborted run must not publish partial JSONL"
    );

    // `committed_chunks: [1]` is a claim about DURABLE STATE, and the manifest
    // asserting it is assembled locally. Every assertion above would still pass
    // if chunk 1's writes had been rolled back or never reached storage, because
    // the bookkeeping would simply agree with itself. Read it back through the
    // same server so the reconciliation record is checked against the database
    // it describes. The stable list contract wraps rows in `items` whether or
    // not the requested limit reaches the entity cap.
    let params = RequestParams {
        plan: None,
        ops: r#"list(kind="concept", limit=200)"#.to_string(),
        presentation: None,
        presentation_per_op: None,
        save_to: None,
        format: Some("json".to_string()),
        format_per_op: None,
        request_id: None,
    };
    let raw = server.dispatch_request_local(params).await.unwrap();
    let response: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let rows = response["results"][0]["result"]["items"]
        .as_array()
        .unwrap_or_else(|| {
            panic!(
                "read-back list result must contain an items array; got {}",
                response["results"][0]["result"]
            )
        });
    // A row that carries no string name is an unreadable instrument, not an
    // absent entity, so it panics here rather than being dropped silently.
    let mut observed: Vec<String> = rows
        .iter()
        .map(|row| {
            row["name"]
                .as_str()
                .unwrap_or_else(|| panic!("read-back row carries no string name: {row}"))
                .to_owned()
        })
        .collect();
    // An empty or unparsed read-back is an instrument failure, not a pass.
    assert!(
        !observed.is_empty(),
        "read-back yielded no rows; result was {}",
        response["results"][0]["result"]
    );
    observed.sort_unstable();

    // Enumerate the two legal outcomes instead of bounding a count. Entity
    // names carry no uniqueness constraint and the upsert is keyed by UUID,
    // so any count of distinct names is a proxy: a duplicate row satisfies
    // it while the property it stands for is broken. Comparing the whole
    // sorted list pins which rows are present, and how many of each.
    let committed: Vec<String> = (0..OPS_FILE_CHUNK_SIZE)
        .map(|index| format!("abort-manifest-{index:03}"))
        .collect();
    // Chunk 2 was dispatched without a verified response, so its single op
    // may or may not have landed. The manifest reports it as unconfirmed
    // rather than committed precisely because both outcomes are legal here.
    let mut with_unconfirmed = committed.clone();
    with_unconfirmed.push(format!("abort-manifest-{OPS_FILE_CHUNK_SIZE:03}"));

    assert!(
        observed == committed || observed == with_unconfirmed,
        "manifest reports chunk 1 committed and chunk 2 unconfirmed, so the database must \
             hold exactly the {OPS_FILE_CHUNK_SIZE} confirmed rows, optionally plus the one \
             unconfirmed row; found {} rows: {observed:?}",
        observed.len()
    );
}

#[tokio::test]
async fn invalid_save_directory_is_rejected_before_any_op_side_effect() {
    if crate::test_process::run_in_child() {
        return;
    }

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();
    let server = isolated_server(&db_path);
    let output_dir = tempfile::tempdir().unwrap();
    let ops = vec![OpsFileEntry {
        tool: "create".to_string(),
        args: serde_json::json!({"kind":"concept","name":"must-not-exist"}),
    }];

    let error = apply_ops_file(
        &server,
        ops,
        Some("verbose".to_string()),
        Some("json".to_string()),
        Some(output_dir.path().to_string_lossy().into_owned()),
        true,
    )
    .await
    .unwrap_err();
    assert!(error
        .to_string()
        .contains("absent or an existing regular file"));

    let params = RequestParams {
        plan: None,
        ops: r#"list(kind="concept")"#.to_string(),
        presentation: None,
        presentation_per_op: None,
        save_to: None,
        format: Some("json".to_string()),
        format_per_op: None,
        request_id: None,
    };
    let raw = server.dispatch_request_local(params).await.unwrap();
    let response: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(
        response["results"][0]["result"]["items"],
        serde_json::json!([])
    );
}

#[tokio::test]
async fn save_manifest_preserves_partial_summary_and_strict_writes_rows() {
    if crate::test_process::run_in_child() {
        return;
    }

    fn partial_ops(success_name: &str) -> Vec<OpsFileEntry> {
        vec![
            OpsFileEntry {
                tool: "create".to_string(),
                args: serde_json::json!({"kind":"concept","name":success_name}),
            },
            OpsFileEntry {
                tool: "search".to_string(),
                args: serde_json::json!({"kind":"not_a_real_kind","query":"x"}),
            },
        ]
    }

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();
    let server = isolated_server(&db_path);
    let output_dir = tempfile::tempdir().unwrap();
    let save_path = output_dir.path().join("partial.jsonl");
    let manifest = apply_ops_file(
        &server,
        partial_ops("partial-ok"),
        Some("verbose".to_string()),
        Some("json".to_string()),
        Some(save_path.to_string_lossy().into_owned()),
        false,
    )
    .await
    .unwrap();
    assert_eq!(manifest["rows"], 2);
    assert_eq!(manifest["summary"]["succeeded"], 1);
    assert_eq!(manifest["summary"]["failed"], 1);
    assert_eq!(manifest["summary"]["aborted"], 0);

    let strict_path = output_dir.path().join("strict-partial.jsonl");
    let error = apply_ops_file(
        &server,
        partial_ops("strict-partial-ok"),
        Some("verbose".to_string()),
        Some("json".to_string()),
        Some(strict_path.to_string_lossy().into_owned()),
        true,
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("--strict"));
    assert_eq!(
        std::fs::read_to_string(strict_path)
            .unwrap()
            .lines()
            .count(),
        2
    );
}

#[tokio::test]
async fn apply_ops_file_strict_errs_when_an_op_fails() {
    if crate::test_process::run_in_child() {
        return;
    }

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();
    let server = isolated_server(&db_path);

    // First op succeeds; second targets an unknown kind and fails.
    let mut f = NamedTempFile::new().unwrap();
    use std::io::Write as _;
    f.write_all(b"{\"tool\":\"create\",\"args\":{\"kind\":\"concept\",\"name\":\"StrictOne\"}}\n")
        .unwrap();
    f.write_all(b"{\"tool\":\"search\",\"args\":{\"kind\":\"not_a_real_kind\",\"query\":\"x\"}}\n")
        .unwrap();

    let ops = parse_ops_file(f.path()).unwrap();
    assert_eq!(ops.len(), 2);

    let err = apply_ops_file(&server, ops, None, None, None, true)
        .await
        .expect_err("strict mode must surface the per-op failure as a process error");
    assert!(format!("{err}").contains("1 op(s) failed"));
}

#[tokio::test]
async fn apply_ops_file_errs_without_strict_when_every_op_fails() {
    if crate::test_process::run_in_child() {
        return;
    }

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();
    let server = isolated_server(&db_path);

    let mut f = NamedTempFile::new().unwrap();
    use std::io::Write as _;
    f.write_all(b"{\"tool\":\"search\",\"args\":{\"kind\":\"not_a_real_kind\",\"query\":\"x\"}}\n")
        .unwrap();

    let ops = parse_ops_file(f.path()).unwrap();
    let err = apply_ops_file(&server, ops, None, None, None, false)
        .await
        .expect_err("a fully-failed ops-file must exit non-zero even without --strict");
    assert!(format!("{err}").contains("every op failed"));
}

// ── ADR-099 B1 inertness (golden shape) ────────────────────────────────────
//
// B1 adds only new, unconsumed types (khive-types atomic admissibility
// metadata, khive-runtime atomic-plan data, khive-request's parse-time
// check). None of them are wired into `dispatch_request_local` or
// `apply_ops_file` — this test pins the non-atomic response envelope's
// shape so a later slice that DOES wire `--atomic` in cannot silently
// change today's default (non-atomic) output. The op sequence below
// (create → update → link → get) is the representative mix named in the
// task: a create, a mutation, a graph edge, and a read, run back-to-back
// through the same in-process dispatch path bulk apply uses.
#[tokio::test]
async fn non_atomic_dispatch_envelope_shape_is_unchanged_by_adr099_b1() {
    if crate::test_process::run_in_child() {
        return;
    }

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();
    let server = isolated_server(&db_path);

    async fn dispatch(server: &KhiveMcpServer, ops: &str) -> serde_json::Value {
        let params = RequestParams {
            plan: None,
            ops: ops.to_string(),
            presentation: None,
            presentation_per_op: None,
            save_to: None,
            format: None,
            format_per_op: None,
            request_id: None,
        };
        let raw = server
            .dispatch_request_local(params)
            .await
            .unwrap_or_else(|e| panic!("dispatch {ops:?} failed: {e}"));
        serde_json::from_str(&raw).expect("valid JSON")
    }

    // create
    let created = dispatch(
        &server,
        r#"create(kind="concept", name="ADR-099-B1-inertness")"#,
    )
    .await;
    assert_golden_envelope_shape(&created, "create");
    let entity_id = created["results"][0]["result"]["id"]
        .as_str()
        .expect("create must return an id")
        .to_string();

    // update
    let updated = dispatch(
        &server,
        &format!(r#"update(id="{entity_id}", description="updated by inertness test")"#),
    )
    .await;
    assert_golden_envelope_shape(&updated, "update");

    // link (self-referential edge is rejected by endpoint validation for
    // most relations, so create a second entity as the link target)
    let target = dispatch(&server, r#"create(kind="concept", name="link-target")"#).await;
    let target_id = target["results"][0]["result"]["id"]
        .as_str()
        .expect("create must return an id")
        .to_string();
    let linked = dispatch(
        &server,
        &format!(r#"link(source_id="{entity_id}", target_id="{target_id}", relation="extends")"#),
    )
    .await;
    assert_golden_envelope_shape(&linked, "link");

    // get (read)
    let got = dispatch(&server, &format!(r#"get(id="{entity_id}")"#)).await;
    assert_golden_envelope_shape(&got, "get");

    // Every op above succeeded end-to-end with zero surprises in the
    // envelope shape — this is the inertness pin: no `atomic` key
    // appeared anywhere, `summary` kept exactly its 4 pre-existing
    // fields on every response, and every op's own result still nests
    // under `results[0].result` as before.
}

#[tokio::test]
async fn ops_file_malformed_line_aborts_before_writes() {
    if crate::test_process::run_in_child() {
        return;
    }

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();

    let mut f = NamedTempFile::new().unwrap();
    use std::io::Write as _;
    // Line 1: valid op
    f.write_all(
        b"{\"tool\":\"create\",\"args\":{\"kind\":\"concept\",\"name\":\"ShouldNotExist\"}}\n",
    )
    .unwrap();
    // Line 2: malformed
    f.write_all(b"INVALID JSON LINE\n").unwrap();

    let path = f.path().to_path_buf();

    // parse_ops_file should fail with line 2 error.
    let err = parse_ops_file(&path).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("line 2"),
        "should report line 2 as malformed: {msg}"
    );

    // Because parse failed, no dispatch happened → DB is clean.
    let server = isolated_server(&db_path);
    let params = RequestParams {
        plan: None,
        ops: r#"list(kind="concept")"#.to_string(),
        presentation: None,
        presentation_per_op: None,
        save_to: None,
        format: None,
        format_per_op: None,
        request_id: None,
    };
    let raw = server.dispatch_request_local(params).await.unwrap();
    let resp: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let count = resp["results"][0]["result"]["items"]
        .as_array()
        .map(|a| a.len())
        .unwrap_or(0);
    assert_eq!(
        count, 0,
        "nothing should be written when any line fails to parse"
    );
}
