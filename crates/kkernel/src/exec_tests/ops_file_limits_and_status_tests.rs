#[test]
fn ops_file_physical_line_and_total_caps_are_fail_closed() {
    let mut within = std::io::Cursor::new(b"1234567\n".to_vec());
    assert_eq!(
        read_bounded_ops_line_with_limit(&mut within, 1, 8)
            .unwrap()
            .unwrap(),
        "1234567"
    );
    let mut over = std::io::Cursor::new(b"12345678\n".to_vec());
    let error = read_bounded_ops_line_with_limit(&mut over, 7, 8).unwrap_err();
    assert!(error.to_string().contains("line 7"));

    let oversized = NamedTempFile::new().unwrap();
    oversized.as_file().set_len(MAX_OPS_FILE_BYTES + 1).unwrap();
    let error = validate_ops_file(oversized.path()).unwrap_err();
    assert!(error.to_string().contains("total limit"));
}

#[test]
fn large_ops_file_payload_is_read_from_path_not_argv() {
    let mut file = NamedTempFile::new().unwrap();
    let payload = "x".repeat(1024 * 1024);
    serde_json::to_writer(
        &mut file,
        &serde_json::json!({"tool":"stats","args":{"payload":payload}}),
    )
    .unwrap();
    file.write_all(b"\n").unwrap();

    let path = file.path().to_str().unwrap();
    let args = ExecArgs::try_parse_from(["exec", "--ops-file", path]).unwrap();
    assert!(args.ops.is_none());
    assert_eq!(args.ops_file.as_deref(), Some(file.path()));
    assert_eq!(validate_ops_file(file.path()).unwrap().total, 1);
}

#[test]
fn chunk_byte_boundary_is_exact() {
    assert!(!should_defer_chunk_entry(
        0,
        0,
        OPS_FILE_CHUNK_MAX_BYTES + 1
    ));
    assert!(!should_defer_chunk_entry(
        1,
        OPS_FILE_CHUNK_MAX_BYTES - 1,
        1
    ));
    assert!(should_defer_chunk_entry(1, OPS_FILE_CHUNK_MAX_BYTES, 1));
}

#[test]
fn ordered_chunk_contract_rejects_tool_or_summary_drift() {
    let tools = vec![
        "first".to_string(),
        "second".to_string(),
        "third".to_string(),
    ];
    let valid = serde_json::json!({
        "results": [
            {"ok":true,"tool":"first","result":{}},
            {"ok":false,"tool":"second","error":"no"},
            {"ok":false,"tool":"third","aborted":true,"error":"not attempted"}
        ],
        "summary":{"total":3,"succeeded":1,"failed":1,"aborted":1},
        "status":"partial"
    });
    assert_eq!(
        validate_ordered_chunk_envelope(&tools, &valid, 1).unwrap(),
        (1, 1, 1)
    );

    let mut wrong_tool = valid.clone();
    wrong_tool["results"][1]["tool"] = serde_json::json!("third");
    assert!(validate_ordered_chunk_envelope(&tools, &wrong_tool, 1).is_err());

    let mut lying_summary = valid;
    lying_summary["summary"]["succeeded"] = serde_json::json!(2);
    lying_summary["summary"]["failed"] = serde_json::json!(0);
    assert!(validate_ordered_chunk_envelope(&tools, &lying_summary, 1).is_err());

    let mut missing_result = serde_json::json!({
        "results": [
            {"ok":true,"tool":"first"},
            {"ok":false,"tool":"second","error":"no"},
            {"ok":false,"tool":"third","aborted":true,"error":"not attempted"}
        ],
        "summary":{"total":3,"succeeded":1,"failed":1,"aborted":1},
        "status":"partial"
    });
    assert!(validate_ordered_chunk_envelope(&tools, &missing_result, 1).is_err());
    missing_result["results"][0]["result"] = serde_json::Value::Null;
    missing_result["results"][1]
        .as_object_mut()
        .unwrap()
        .remove("error");
    assert!(validate_ordered_chunk_envelope(&tools, &missing_result, 1).is_err());

    let mut contradictory = serde_json::json!({
        "results": [
            {"ok":true,"tool":"first","result":null,"error":null},
            {"ok":false,"tool":"second","error":"no","result":null},
            {"ok":false,"tool":"third","aborted":true,"error":"not attempted"}
        ],
        "summary":{"total":3,"succeeded":1,"failed":1,"aborted":1},
        "status":"partial"
    });
    assert!(validate_ordered_chunk_envelope(&tools, &contradictory, 1).is_err());
    contradictory["results"][0]
        .as_object_mut()
        .unwrap()
        .remove("error");
    assert!(validate_ordered_chunk_envelope(&tools, &contradictory, 1).is_err());
}

fn status_contract_fixture(status: &str) -> (Vec<String>, serde_json::Value) {
    (
        vec!["first".to_string(), "second".to_string()],
        serde_json::json!({
            "results": [
                {"ok":true,"tool":"first","result":{}},
                {"ok":false,"tool":"second","error":"no"}
            ],
            "summary":{"total":2,"succeeded":1,"failed":1,"aborted":0},
            "status":status
        }),
    )
}

#[test]
fn ordered_chunk_truthful_status_passes() {
    let (ops, envelope) = status_contract_fixture("partial");
    assert_eq!(
        validate_ordered_chunk_envelope(&ops, &envelope, 1).unwrap(),
        (1, 1, 0)
    );
}

#[test]
fn ordered_chunk_contradicting_status_is_rejected() {
    let (ops, envelope) = status_contract_fixture("success");
    let error = validate_ordered_chunk_envelope(&ops, &envelope, 1).unwrap_err();
    assert!(error.to_string().contains("status"));
}
