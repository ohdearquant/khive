use clap::Parser;
use kkernel::exec::{run_exec, ExecArgs};
use tempfile::TempDir;

fn args(root: &TempDir, ops: &str) -> ExecArgs {
    let db = root.path().join("missing.db");
    let config = root.path().join("khive.toml");
    std::fs::write(&config, "engines = []\n")
        .expect("write isolated config");
    ExecArgs::try_parse_from([
        "exec",
        ops,
        "--read-only",
        "--db",
        db.to_str().expect("utf8 fixture path"),
        "--config",
        config.to_str().expect("utf8 config path"),
        "--actor",
        "actor:read-only-test",
    ])
    .expect("parse readonly arguments")
}

async fn assert_refusal(args: ExecArgs, message: &str, root: &TempDir) {
    let error = run_exec(args).await.expect_err("invocation must be refused");
    assert!(error.to_string().contains(message), "{error:#}");
    assert!(!root.path().join("missing.db").exists(), "opened a writer");
    assert!(!root.path().join("output.jsonl").exists(), "reserved save output");
}

#[tokio::test]
async fn read_only_refuses_plan_for_direct_callers_before_configuration() {
    let root = tempfile::tempdir().unwrap();
    let mut invocation = args(&root, "whoami()");
    invocation.plan = true;
    invocation.config = Some(root.path().join("absent-config.toml"));
    assert_refusal(invocation, "--read-only conflicts with --plan", &root).await;
}

#[tokio::test]
async fn read_only_refuses_ops_file_before_opening_the_input() {
    let root = tempfile::tempdir().unwrap();
    let mut invocation = args(&root, "whoami()");
    invocation.ops = None;
    invocation.ops_file = Some(root.path().join("absent-input.jsonl"));
    assert_refusal(invocation, "--read-only conflicts with --ops-file", &root).await;
}

#[tokio::test]
async fn read_only_refuses_pending_events_before_running_the_drain() {
    let root = tempfile::tempdir().unwrap();
    let mut invocation = args(&root, "whoami()");
    invocation.ops = None;
    invocation.pending_events = true;
    assert_refusal(invocation, "--read-only conflicts with --pending-events", &root).await;
}

#[tokio::test]
async fn read_only_refuses_writes_unknowns_and_mixed_requests_before_boot() {
    for ops in [
        "create(kind=\"concept\", name=\"must-not-write\")",
        "unknown_read_only_probe()",
        "[whoami(), create(kind=\"concept\", name=\"must-not-write\")]",
        "whoami() | create(kind=\"concept\", name=\"must-not-write\")",
    ] {
        let root = tempfile::tempdir().unwrap();
        let mut invocation = args(&root, ops);
        invocation.save_file = Some(root.path().join("output.jsonl").display().to_string());
        invocation.config = Some(root.path().join("absent-config.toml"));
        assert_refusal(invocation, "--read-only refuses operation", &root).await;
    }
}

#[tokio::test]
async fn read_only_refuses_memory_storage() {
    let root = tempfile::tempdir().unwrap();
    let mut invocation = args(&root, "whoami()");
    invocation.db = Some(":memory:".into());
    assert_refusal(invocation, "--read-only requires an existing file-backed database", &root)
        .await;
}

#[tokio::test]
async fn read_only_refuses_a_missing_database_without_creating_it() {
    let root = tempfile::tempdir().unwrap();
    assert_refusal(
        args(&root, "whoami()"),
        "--read-only requires an existing file-backed database",
        &root,
    )
    .await;
}

#[tokio::test]
async fn read_only_refuses_any_declared_backends_before_opening_storage() {
    for backend_count in [1, 2] {
        let root = tempfile::tempdir().unwrap();
        let invocation = args(&root, "whoami()");
        let other = root.path().join("other.db");
        let mut config = format!(
            "engines = []\n\n[[backends]]\nname = \"main\"\nkind = \"sqlite\"\npath = \"{}\"\n",
            root.path().join("missing.db").display(),
        );
        if backend_count == 2 {
            config.push_str(&format!(
                "\n[[backends]]\nname = \"other\"\nkind = \"sqlite\"\npath = \"{}\"\n",
                other.display(),
            ));
        }
        std::fs::write(invocation.config.as_ref().unwrap(), config).unwrap();
        assert_refusal(invocation, "--read-only does not support declared backends", &root).await;
        assert!(!other.exists(), "opened the secondary backend");
    }
}

#[tokio::test]
async fn read_only_refuses_configured_mounts_before_starting_a_producer() {
    let root = tempfile::tempdir().unwrap();
    let invocation = args(&root, "whoami()");
    std::fs::write(
        invocation.config.as_ref().unwrap(),
        "engines = []\n\n[[mounts]]\nname = \"fixture-mount\"\ntransport = \"stdio\"\ncommand = \"/bin/true\"\n",
    )
    .unwrap();
    assert_refusal(invocation, "--read-only does not support configured mounts", &root).await;
}
