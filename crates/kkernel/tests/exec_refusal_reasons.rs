//! Black-box contract tests for `kkernel exec` refusal classifications (#1456).

use std::process::{Command, Output};
use std::sync::OnceLock;

use sha2::{Digest, Sha256};
use tempfile::TempDir;

fn kkernel_bin() -> &'static str {
    env!("CARGO_BIN_EXE_kkernel")
}

fn configured_exec(home: &TempDir, packs: &str) -> Command {
    let mut command = Command::new(kkernel_bin());
    command
        .arg("exec")
        .current_dir(home.path())
        .env("HOME", home.path())
        .env("KHIVE_NO_DAEMON", "1")
        .env("KHIVE_PACKS", packs)
        .env("RUST_LOG", "error")
        .env_remove("KHIVE_ACTOR")
        .env_remove("KHIVE_ADDITIONAL_EMBEDDING_MODELS")
        .env_remove("KHIVE_CONFIG")
        .env_remove("KHIVE_DAEMON_STRICT")
        .env_remove("KHIVE_DB")
        .env_remove("KHIVE_EMBEDDING_MODEL")
        .env_remove("KHIVE_OUTPUT_FORMAT")
        .env_remove("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");
    command
}

fn run_exec_at_db(
    home: &TempDir,
    ops: &str,
    packs: &str,
    db: &str,
    identity_args: &[&str],
    extra_args: &[&str],
    require_attributed_actor: bool,
) -> Output {
    let mut command = configured_exec(home, packs);
    command
        .arg(ops)
        .args(["--db", db])
        .args(identity_args)
        .args(extra_args);
    if require_attributed_actor {
        command.env("KHIVE_REQUIRE_ATTRIBUTED_ACTOR", "1");
    }
    command.output().expect("run kkernel exec")
}

fn run_exec(
    home: &TempDir,
    ops: &str,
    packs: &str,
    identity_args: &[&str],
    require_attributed_actor: bool,
) -> Output {
    run_exec_at_db(
        home,
        ops,
        packs,
        ":memory:",
        identity_args,
        &[],
        require_attributed_actor,
    )
}

#[allow(clippy::too_many_arguments)]
fn run_ops_file(
    home: &TempDir,
    filename: &str,
    contents: &str,
    packs: &str,
    db: &str,
    identity_args: &[&str],
    extra_args: &[&str],
    require_attributed_actor: bool,
) -> Output {
    let path = home.path().join(filename);
    std::fs::write(&path, contents).expect("write ops-file fixture");
    let mut command = configured_exec(home, packs);
    command
        .arg("--ops-file")
        .arg(path)
        .args(["--db", db])
        .args(identity_args)
        .args(extra_args);
    if require_attributed_actor {
        command.env("KHIVE_REQUIRE_ATTRIBUTED_ACTOR", "1");
    }
    command.output().expect("run kkernel exec --ops-file")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout_json(output: &Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout was not JSON: {error}; stdout={:?}; stderr={:?}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn assert_refusal_once(output: &Output, expected: &str) {
    assert_refusal_count(output, expected, 1);
}

fn assert_refusal_count(output: &Output, expected: &str, expected_count: usize) {
    let expected = format!("kkernel-refusal: {expected}");
    let stderr = stderr(output);
    let count = stderr.lines().filter(|line| *line == expected).count();
    assert_eq!(count, expected_count, "stderr={stderr:?}");
}

/// Share the one runtime boot needed to exercise all three per-op refusal
/// classes. Results remain in input order even though the batch dispatches in
/// parallel, so each test can pin its own path independently.
fn classified_batch_output() -> &'static Output {
    static OUTPUT: OnceLock<Output> = OnceLock::new();
    OUTPUT.get_or_init(|| {
        let home = TempDir::new().unwrap();
        run_exec(
            &home,
            r#"[stats(), get(), not_loaded(), create(kind="concept", name="AKIAFAKEKEY1234567890")]"#,
            "kg",
            &["--actor", "lambda:test", "--strict"],
            false,
        )
    })
}

#[test]
fn anonymous_actor_refusal_has_stable_token_and_json_reason() {
    let home = TempDir::new().unwrap();
    let output = run_exec(&home, "stats()", "kg,comm", &[], true);
    assert!(!output.status.success());
    assert_refusal_once(&output, "anonymous-actor");
    assert!(stderr(&output).contains("KHIVE_REQUIRE_ATTRIBUTED_ACTOR=1"));
    let response = stdout_json(&output);
    assert_eq!(response["results"][0]["tool"], "stats");
    assert_eq!(response["results"][0]["reason"], "anonymous-actor");
}

#[test]
fn expected_actor_mismatch_has_stable_token_and_json_reason() {
    let home = TempDir::new().unwrap();
    let output = run_exec(
        &home,
        "stats()",
        "kg",
        &[
            "--actor",
            "lambda:actual",
            "--expect-actor",
            "lambda:expected",
        ],
        false,
    );
    assert!(!output.status.success());
    assert_refusal_once(&output, "expect-actor-mismatch");
    assert!(stderr(&output).contains("--expect-actor mismatch"));
    let response = stdout_json(&output);
    assert_eq!(response["results"][0]["tool"], "stats");
    assert_eq!(response["results"][0]["reason"], "expect-actor-mismatch");
}

#[test]
fn secret_gate_refusal_has_stable_token_and_json_reason() {
    let output = classified_batch_output();
    assert!(!output.status.success());
    assert_refusal_once(output, "gate-refusal");
    let response = stdout_json(output);
    assert_eq!(response["results"][3]["reason"], "gate-refusal");
    assert!(response["results"][3]["error"]
        .as_str()
        .is_some_and(|error| error.contains("write blocked")));
}

#[test]
fn strict_batch_failure_has_stable_token_and_json_reason() {
    let output = classified_batch_output();
    assert!(!output.status.success());
    assert_refusal_once(output, "strict-op-failure");
    let response = stdout_json(output);
    assert_eq!(response["summary"]["succeeded"], 1);
    assert_eq!(response["results"][1]["reason"], "strict-op-failure");
    assert!(stderr(output).contains("--strict"));
}

#[test]
fn parse_error_has_stable_token_and_json_reason() {
    let home = TempDir::new().unwrap();
    let output = run_exec(&home, "stats(", "kg", &["--actor", "lambda:test"], false);
    assert!(!output.status.success());
    assert_refusal_once(&output, "parse-error");
    let response = stdout_json(&output);
    assert_eq!(response["error"]["code"], "invalid_params");
    assert_eq!(response["error"]["reason"], "parse-error");
    assert_eq!(response["invocation"]["started"], false);
    assert!(response.get("results").is_none());
}

#[test]
fn malformed_ops_file_with_valid_prefix_is_an_invocation_error_not_a_fake_op() {
    let home = TempDir::new().unwrap();
    let db_path = home.path().join("must-not-open.sqlite");
    let db = db_path.to_string_lossy().into_owned();
    let output = run_ops_file(
        &home,
        "malformed.jsonl",
        "{\"tool\":\"create\",\"args\":{\"kind\":\"concept\",\"name\":\"must-not-run\"}}\nnot-json\n",
        "kg",
        &db,
        &["--actor", "lambda:test"],
        &[],
        false,
    );
    assert!(!output.status.success());
    assert_refusal_once(&output, "parse-error");
    let response = stdout_json(&output);
    assert_eq!(response["error"]["code"], "invalid_params");
    assert_eq!(response["error"]["reason"], "parse-error");
    assert_eq!(response["invocation"]["started"], false);
    assert!(response.get("results").is_none());
    assert!(response["error"]["message"]
        .as_str()
        .is_some_and(|message| message.contains("line 2")));
    assert!(
        !db_path.exists(),
        "parse-before-dispatch must not even open the target database"
    );
}

#[test]
fn parse_error_precedes_anonymous_actor_for_both_input_carriers() {
    let inline_home = TempDir::new().unwrap();
    let inline = run_exec(&inline_home, "stats(", "kg,comm", &[], true);
    assert!(!inline.status.success());
    assert_refusal_once(&inline, "parse-error");
    assert_refusal_count(&inline, "anonymous-actor", 0);
    let inline_response = stdout_json(&inline);
    assert_eq!(inline_response["error"]["code"], "invalid_params");
    assert_eq!(inline_response["error"]["reason"], "parse-error");

    let file_home = TempDir::new().unwrap();
    let file = run_ops_file(
        &file_home,
        "anonymous-malformed.jsonl",
        "not-json\n",
        "kg,comm",
        ":memory:",
        &[],
        &[],
        true,
    );
    assert!(!file.status.success());
    assert_refusal_once(&file, "parse-error");
    assert_refusal_count(&file, "anonymous-actor", 0);
    let file_response = stdout_json(&file);
    assert_eq!(file_response["error"]["code"], "invalid_params");
    assert_eq!(file_response["error"]["reason"], "parse-error");
}

#[test]
fn parse_error_precedes_expect_actor_mismatch() {
    let inline_home = TempDir::new().unwrap();
    let identity_args = [
        "--actor",
        "lambda:actual",
        "--expect-actor",
        "lambda:expected",
    ];
    let inline = run_exec(&inline_home, "stats(", "kg", &identity_args, false);
    assert!(!inline.status.success());
    assert_refusal_once(&inline, "parse-error");
    assert_refusal_count(&inline, "expect-actor-mismatch", 0);
    let inline_response = stdout_json(&inline);
    assert_eq!(inline_response["error"]["code"], "invalid_params");
    assert_eq!(inline_response["error"]["reason"], "parse-error");

    let file_home = TempDir::new().unwrap();
    let file = run_ops_file(
        &file_home,
        "actor-mismatch-malformed.jsonl",
        "not-json\n",
        "kg",
        ":memory:",
        &identity_args,
        &[],
        false,
    );
    assert!(!file.status.success());
    assert_refusal_once(&file, "parse-error");
    assert_refusal_count(&file, "expect-actor-mismatch", 0);
    let file_response = stdout_json(&file);
    assert_eq!(file_response["error"]["code"], "invalid_params");
    assert_eq!(file_response["error"]["reason"], "parse-error");
}

#[test]
fn ops_file_actor_mismatch_reports_every_real_operation() {
    let home = TempDir::new().unwrap();
    let db_path = home.path().join("actor-mismatch-must-not-open.sqlite");
    let db = db_path.to_string_lossy().into_owned();
    let output = run_ops_file(
        &home,
        "actor-mismatch.jsonl",
        "{\"tool\":\"stats\",\"args\":{}}\n{\"tool\":\"get\",\"args\":{}}\n",
        "kg",
        &db,
        &[
            "--actor",
            "lambda:actual",
            "--expect-actor",
            "lambda:expected",
        ],
        &[],
        false,
    );
    assert!(!output.status.success());
    assert_refusal_once(&output, "expect-actor-mismatch");
    let response = stdout_json(&output);
    assert_eq!(response["summary"]["total"], 2);
    assert_eq!(response["results"][0]["tool"], "stats");
    assert_eq!(response["results"][1]["tool"], "get");
    assert!(response["results"]
        .as_array()
        .expect("results array")
        .iter()
        .all(|entry| entry["reason"] == "expect-actor-mismatch"));
    assert!(
        !db_path.exists(),
        "actor mismatch may parse descriptors but must not open the target database"
    );
}

#[test]
fn unknown_verb_has_stable_token_and_json_reason() {
    let output = classified_batch_output();
    assert!(!output.status.success());
    assert_refusal_once(output, "verb-refused");
    let response = stdout_json(output);
    assert_eq!(response["results"][2]["reason"], "verb-refused");
    assert!(response["results"][2]["error"]
        .as_str()
        .is_some_and(|error| error.contains("unknown verb")));
}

#[test]
fn successful_exec_emits_no_refusal_token_or_json_reason() {
    let home = TempDir::new().unwrap();
    let output = run_exec(&home, "stats()", "kg", &["--actor", "lambda:test"], false);
    assert!(output.status.success(), "stderr={}", stderr(&output));
    assert!(!stderr(&output).contains("kkernel-refusal:"));
    let response = stdout_json(&output);
    assert_eq!(response["status"], "success");
    assert!(response["results"][0].get("reason").is_none());
}

struct AtomicScenarioOutputs {
    unknown_and_unloaded: Output,
    known_ineligible: Output,
    target_exists_after_preflight_refusals: bool,
    secret_gate: Output,
    strict_rollback: Output,
    success: Output,
}

fn bulk_entity_id(response: &serde_json::Value, index: usize) -> String {
    response["results"][0]["result"]["entities"][index]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("missing bulk entity id at index {index}: {response}"))
        .to_string()
}

/// Share the persistent database boot and seed work across all atomic contract
/// assertions. Each scenario is still a real black-box `kkernel` invocation.
fn atomic_scenario_outputs() -> &'static AtomicScenarioOutputs {
    static OUTPUTS: OnceLock<AtomicScenarioOutputs> = OnceLock::new();
    OUTPUTS.get_or_init(|| {
        let home = TempDir::new().expect("atomic fixture home");
        let db = home.path().join("atomic.sqlite");
        let db = db.to_string_lossy().into_owned();

        let unknown_and_unloaded = run_ops_file(
            &home,
            "atomic-unknown.jsonl",
            "{\"tool\":\"not_loaded\",\"args\":{}}\n{\"tool\":\"gtd.transition\",\"args\":{\"id\":\"00000000-0000-0000-0000-000000000000\",\"status\":\"next\"}}\n",
            "kg",
            &db,
            &["--actor", "lambda:test"],
            &["--atomic"],
            false,
        );
        let known_ineligible = run_ops_file(
            &home,
            "atomic-ineligible.jsonl",
            "{\"tool\":\"stats\",\"args\":{}}\n",
            "kg",
            &db,
            &["--actor", "lambda:test"],
            &["--atomic"],
            false,
        );
        let target_exists_after_preflight_refusals = std::path::Path::new(&db).exists();

        let seed = run_exec_at_db(
            &home,
            r#"create(items=[{"kind":"concept", "name":"AtomicRollbackX"}, {"kind":"concept", "name":"AtomicRollbackY"}], verbose=true)"#,
            "kg",
            &db,
            &["--actor", "lambda:test"],
            &[],
            false,
        );
        assert!(seed.status.success(), "seed stderr={}", stderr(&seed));
        let seed_response = stdout_json(&seed);
        let x_id = bulk_entity_id(&seed_response, 0);
        let y_id = bulk_entity_id(&seed_response, 1);

        let secret_contents = format!(
            "{}\n",
            serde_json::json!({
                "tool": "gtd.transition",
                "args": {
                    // The secret gate runs before task lookup, so a sentinel
                    // ID keeps this refusal test independent of note creation
                    // and of any configured/downloaded embedding model.
                    "id": "00000000-0000-0000-0000-000000000000",
                    "status": "active",
                    "note": "leaked key AKIAFAKEKEY1234567890"
                }
            })
        );
        let secret_gate = run_ops_file(
            &home,
            "atomic-secret.jsonl",
            &secret_contents,
            "kg,gtd",
            &db,
            &["--actor", "lambda:test"],
            &["--atomic"],
            false,
        );

        let rollback_contents = format!(
            "{}\n{}\n",
            serde_json::json!({"tool": "delete", "args": {"id": x_id.as_str(), "hard": true}}),
            // Both plans prepare against the still-present row. The first
            // delete succeeds inside the transaction; the second observes
            // zero affected rows and deterministically trips its exact-one
            // guard, forcing the whole unit to roll back.
            serde_json::json!({"tool": "delete", "args": {"id": x_id.as_str(), "hard": true}})
        );
        let strict_rollback = run_ops_file(
            &home,
            "atomic-rollback.jsonl",
            &rollback_contents,
            "kg",
            &db,
            &["--actor", "lambda:test"],
            &["--atomic", "--strict"],
            false,
        );

        let success_contents = format!(
            "{}\n",
            serde_json::json!({
                "tool": "update",
                "args": {"id": y_id.as_str(), "name": "AtomicRollbackY-updated"}
            })
        );
        let success = run_ops_file(
            &home,
            "atomic-success.jsonl",
            &success_contents,
            "kg",
            &db,
            &["--actor", "lambda:test"],
            &["--atomic"],
            false,
        );

        AtomicScenarioOutputs {
            unknown_and_unloaded,
            known_ineligible,
            target_exists_after_preflight_refusals,
            secret_gate,
            strict_rollback,
            success,
        }
    })
}

#[test]
fn atomic_unknown_and_unloaded_verbs_are_typed_per_operation() {
    let scenarios = atomic_scenario_outputs();
    let output = &scenarios.unknown_and_unloaded;
    assert!(!output.status.success());
    assert_refusal_count(output, "verb-refused", 2);
    let response = stdout_json(output);
    assert_eq!(response["summary"]["total"], 2);
    assert_eq!(response["results"][0]["tool"], "not_loaded");
    assert_eq!(response["results"][1]["tool"], "gtd.transition");
    assert!(response["results"]
        .as_array()
        .expect("results array")
        .iter()
        .all(|entry| entry["reason"] == "verb-refused"));
    assert!(
        !scenarios.target_exists_after_preflight_refusals,
        "atomic preflight refusals must not open the target database"
    );
}

#[test]
fn atomic_known_but_ineligible_verb_is_not_mislabeled_unknown() {
    let output = &atomic_scenario_outputs().known_ineligible;
    assert!(!output.status.success());
    assert!(!stderr(output).contains("kkernel-refusal:"));
    let response = stdout_json(output);
    assert_eq!(response["results"][0]["tool"], "stats");
    assert!(response["results"][0].get("reason").is_none());
    assert!(response["results"][0]["error"]
        .as_str()
        .is_some_and(|error| error.contains("not atomic-admissible")));
}

#[test]
fn atomic_secret_prepare_refusal_has_gate_reason() {
    let output = &atomic_scenario_outputs().secret_gate;
    assert!(!output.status.success());
    assert_refusal_once(output, "gate-refusal");
    let response = stdout_json(output);
    assert_eq!(response["results"][0]["tool"], "gtd.transition");
    assert_eq!(response["results"][0]["reason"], "gate-refusal");
    assert!(response["results"][0]["error"]
        .as_str()
        .is_some_and(|error| error.contains("write blocked")));
}

#[test]
fn atomic_strict_rollback_classifies_each_not_committed_operation() {
    let output = &atomic_scenario_outputs().strict_rollback;
    // `--strict` was historically documented as not affecting atomic exit
    // status. #1456 adds classification without changing that process status.
    assert!(output.status.success(), "stderr={}", stderr(output));
    assert_refusal_count(output, "strict-op-failure", 2);
    let response = stdout_json(output);
    assert_eq!(response["atomic"]["rolled_back"], true);
    assert_eq!(response["atomic"]["failed_op_index"], 1);
    assert!(response["atomic"]["error"]
        .as_str()
        .is_some_and(|error| error.contains("guard failed")));
    assert!(response["results"]
        .as_array()
        .expect("results array")
        .iter()
        .all(|entry| entry["reason"] == "strict-op-failure"));
}

#[test]
fn atomic_success_emits_no_refusal_token_or_reason() {
    let output = &atomic_scenario_outputs().success;
    assert!(output.status.success(), "stderr={}", stderr(output));
    assert!(!stderr(output).contains("kkernel-refusal:"));
    let response = stdout_json(output);
    assert_eq!(response["atomic"]["committed"], true);
    assert_eq!(
        response["results"][0]["result"]["name"],
        "AtomicRollbackY-updated"
    );
    assert!(response["results"][0].get("reason").is_none());
}

#[test]
fn save_file_manifest_preserves_specific_and_strict_reasons() {
    let home = TempDir::new().unwrap();
    let save_path = home.path().join("results.jsonl");
    let save_path_arg = save_path.to_string_lossy().into_owned();
    let output = run_exec_at_db(
        &home,
        "[stats(), get(), not_loaded()]",
        "kg",
        ":memory:",
        &["--actor", "lambda:test"],
        &["--strict", "--save-file", &save_path_arg],
        false,
    );
    assert!(!output.status.success());
    assert_refusal_once(&output, "strict-op-failure");
    assert_refusal_once(&output, "verb-refused");
    let manifest = stdout_json(&output);
    assert_eq!(manifest["summary"]["failed"], 2);
    assert_eq!(manifest["failures"][0]["reason"], "strict-op-failure");
    assert_eq!(manifest["failures"][1]["reason"], "verb-refused");

    let rows = std::fs::read_to_string(save_path).expect("read saved JSONL rows");
    let checksum = format!("{:x}", Sha256::digest(rows.as_bytes()));
    assert_eq!(manifest["checksum"], checksum);
    let parsed_rows = rows
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("saved JSON row"))
        .collect::<Vec<_>>();
    assert_eq!(parsed_rows.len(), 3);
    assert_eq!(parsed_rows[1]["reason"], "strict-op-failure");
    assert_eq!(parsed_rows[2]["reason"], "verb-refused");
    for failure in manifest["failures"]
        .as_array()
        .expect("manifest failure projection")
    {
        let op_index = failure["op_index"].as_u64().expect("failure op index") as usize;
        assert_eq!(
            failure["reason"], parsed_rows[op_index]["reason"],
            "manifest reason must project the canonical checksummed JSONL row"
        );
    }
}
