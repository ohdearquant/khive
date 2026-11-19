use super::*;
use khive_storage::test_support::run_exact_test_in_child;
use std::panic::{catch_unwind, AssertUnwindSafe};

const FIXTURE_ROOT: &str = "KKERNEL_REINDEX_CLAMP_FIXTURE";

fn capture_in_child(expected_configured: Option<u64>) -> bool {
    let mut fixture = None;
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        run_exact_test_in_child("KKERNEL_REINDEX_CLAMP_CHILD", false, |command| {
            let root = tempfile::tempdir().expect("private clamp fixture");
            let home = root.path().join("home");
            std::fs::create_dir(&home).unwrap();
            for (key, _) in std::env::vars_os() {
                if key.to_string_lossy().starts_with("KHIVE_") {
                    command.env_remove(key);
                }
            }
            command
                .env(FIXTURE_ROOT, root.path())
                .env("HOME", home)
                .env_remove("LATTICE_MODEL_CACHE")
                .env("KHIVE_TEST_HARNESS", "1")
                .env("KHIVE_VOLUME_LOCK_DIR", root.path().join("volume-locks"))
                .env("KHIVE_LOCK", root.path().join("recovery.lock"))
                .stderr(std::fs::File::create(root.path().join("stderr")).unwrap());
            fixture = Some(root);
        })
    }));
    if matches!(outcome, Ok(false)) {
        return false;
    }
    let root = fixture.expect("parent owns child fixture");
    let stderr = std::fs::read_to_string(root.path().join("stderr")).unwrap();
    assert!(matches!(outcome, Ok(true)), "child failed:\n{stderr}");
    let warnings: Vec<_> = stderr
        .lines()
        .filter(|line| line.contains("embed.batch_size"))
        .collect();
    let expected: Vec<_> = expected_configured
        .map(|configured| {
            format!(
                "warning: {}: configured embed.batch_size={configured} exceeds 500; using 500",
                root.path()
                    .join("selected.toml")
                    .canonicalize()
                    .unwrap()
                    .display()
            )
        })
        .into_iter()
        .collect();
    assert_eq!(warnings, expected, "captured driver stderr:\n{stderr}");
    assert!(
        std::fs::read_dir(root.path().join("home"))
            .unwrap()
            .next()
            .is_none(),
        "fixture must not download models or touch child HOME"
    );
    true
}

fn selected(configured: Option<u64>) -> PathBuf {
    let root = PathBuf::from(std::env::var_os(FIXTURE_ROOT).expect("isolated child root"));
    let path = root.join("selected.toml");
    let content = configured
        .map(|batch| format!("[embed]\nbatch_size={batch}\n"))
        .unwrap_or_default();
    std::fs::write(&path, content).unwrap();
    path
}

async fn run_without_models(args: ReindexArgs) -> Result<()> {
    run_reindex_with_setup(
        args,
        |mut cfg| {
            cfg.disable_embedding_models();
            cfg
        },
        |runtime| {
            assert!(runtime.backend().pool().canonical_path().is_none());
            assert!(runtime.registered_embedding_model_names().is_empty());
            Ok(())
        },
    )
    .await
}

async fn run_bulk(configured: Option<u64>, cli: Option<u32>, expected: u32) {
    let path = selected(configured);
    assert_eq!(
        resolve_reindex_configuration(Some(":memory:"), Some(&path), cli)
            .unwrap()
            .1,
        expected
    );
    let mut args = ReindexArgs::try_parse_from([
        "reindex",
        "--db",
        ":memory:",
        "--config",
        path.to_str().unwrap(),
        "--namespace",
        "local",
        "--no-knowledge",
    ])
    .unwrap();
    args.batch_size = cli;
    run_without_models(args).await.unwrap();
}

#[tokio::test]
async fn configured_batch_above_limit_warns_once_with_selected_file_and_effective_value() {
    if capture_in_child(Some(901)) {
        return;
    }
    run_bulk(Some(901), None, 500).await;
}

#[tokio::test]
async fn configured_boundary_in_range_and_default_batches_are_silent() {
    if capture_in_child(None) {
        return;
    }
    for (configured, expected) in [(Some(500), 500), (Some(64), 64), (None, 128)] {
        run_bulk(configured, None, expected).await;
    }
}

#[tokio::test]
async fn explicit_cli_batches_do_not_warn_about_shadowed_configuration() {
    if capture_in_child(None) {
        return;
    }
    for (cli, expected) in [(64, 64), (900, 500), (0, 1), (128, 128)] {
        run_bulk(Some(901), Some(cli), expected).await;
    }
}

#[tokio::test]
async fn validation_and_single_record_paths_do_not_warn_about_an_unused_batch() {
    if capture_in_child(None) {
        return;
    }
    let path = selected(Some(901));
    assert!(
        validate_declared_reindex_target(Some(":memory:"), Some(&path))
            .unwrap()
            .is_none()
    );
    let missing = Uuid::new_v4();
    let args = ReindexArgs::try_parse_from([
        "reindex",
        "--db",
        ":memory:",
        "--config",
        path.to_str().unwrap(),
        "--namespace",
        "local",
        "--id",
        &missing.to_string(),
    ])
    .unwrap();
    let error = run_without_models(args).await.expect_err("missing record");
    assert!(
        matches!(
            error.downcast_ref::<khive_runtime::RuntimeError>(),
            Some(khive_runtime::RuntimeError::NotFound(message))
                if message == &format!("live entity or note {missing} in namespace local")
        ),
        "single-record refusal changed: {error:#}"
    );
}
