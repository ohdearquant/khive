//! Deterministic cross-process renewal at the writer transaction boundary.
use super::*;
use khive_storage::{AtomicUnitOp, SqlAccess, SqlReader, SqlWriter, StorageError, StorageResult};
use std::any::Any;
use std::path::PathBuf;

fn file_runtime(path: PathBuf) -> KhiveRuntime {
    let runtime = KhiveRuntime::new(crate::RuntimeConfig {
        db_path: Some(path),
        embedding_model: None,
        additional_embedding_models: vec![],
        ..Default::default()
    })
    .unwrap();
    runtime.install_kind_registry(vec![], vec!["head".into()]);
    runtime
}

#[tokio::test]
#[ignore = "subprocess helper, invoked by the transaction-boundary test"]
async fn ordered_fences_renewal_process() {
    let path = std::env::var_os("KHIVE_FENCE_TEST_DB").expect("isolated child database");
    let id = std::env::var("KHIVE_FENCE_TEST_ID")
        .unwrap()
        .parse()
        .unwrap();
    let runtime = file_runtime(path.into());
    let token = runtime.authorize(khive_types::Namespace::local()).unwrap();
    let note = runtime
        .update_note(&token, id, patch("{\"renewed\":true}", 1, None))
        .await
        .unwrap();
    assert_eq!(note.version, 2);
    println!("RENEWED_BY_PROCESS {}", std::process::id());
}

/// The storage seam is real. Only its admission point schedules the competing
/// process, so no timing assumption or production-only hook decides the race.
struct RenewBeforeBegin {
    inner: Arc<dyn SqlAccess>,
    path: PathBuf,
    id: uuid::Uuid,
}

#[async_trait]
impl SqlAccess for RenewBeforeBegin {
    async fn reader(&self) -> StorageResult<Box<dyn SqlReader>> {
        self.inner.reader().await
    }
    async fn writer(&self) -> StorageResult<Box<dyn SqlWriter>> {
        self.inner.writer().await
    }
    async fn atomic_unit(&self, op: AtomicUnitOp) -> StorageResult<Box<dyn Any + Send>> {
        let path = self.path.clone();
        let id = self.id;
        tokio::task::spawn_blocking(move || {
            use std::process::{Command, Stdio};
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "note_write_tests::fence_races::ordered_fences_renewal_process",
                    "--ignored",
                    "--nocapture",
                ])
                .env("KHIVE_FENCE_TEST_DB", path)
                .env("KHIVE_FENCE_TEST_ID", id.to_string())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            assert_ne!(child.id(), std::process::id());
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            loop {
                if child.try_wait().unwrap().is_some() {
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    child.kill().unwrap();
                    let output = child.wait_with_output().unwrap();
                    panic!("renewal process timed out: {output:?}");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let output = child.wait_with_output().unwrap();
            assert!(output.status.success(), "{output:?}");
            assert!(
                String::from_utf8_lossy(&output.stdout).contains("RENEWED_BY_PROCESS"),
                "{output:?}"
            );
        })
        .await
        .map_err(|error| StorageError::Internal(error.to_string()))?;
        self.inner.atomic_unit(op).await
    }
}

#[tokio::test]
async fn ordered_fences_cross_process_renewal_before_begin() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fences.db");
    let runtime = file_runtime(path.clone());
    let token = runtime.authorize(khive_types::Namespace::local()).unwrap();
    let a = create(&runtime, &token, "lease/a", None).await;
    let b = create(&runtime, &token, "lease/b", None).await;
    let target = create(&runtime, &token, "target", None).await;
    let plan = crate::atomic_prepare::prepare_update(
        &runtime,
        &token,
        &json!({"id":target.id,"content":"{\"bad\":true}","expected_version":1,"fence":[
            {"key":"lease/a","kind":"head","expected_version":1},
            {"key":"lease/b","kind":"head","expected_version":1}]}),
        None,
    )
    .await
    .unwrap();
    let access = RenewBeforeBegin {
        inner: runtime.sql(),
        path,
        id: b.id,
    };
    let outcome = run_atomic_unit(&access, vec![plan]).await.unwrap();
    let AtomicRunOutcome::RolledBack {
        failure: crate::atomic_runner::AtomicOpFailure::NoteConflict(conflict),
        ..
    } = outcome
    else {
        panic!("must refuse lease renewed by another process: {outcome:?}");
    };
    let error = serde_json::to_value(conflict.into_error()).unwrap();
    assert_eq!(
        error["details"],
        json!({"reason":"fence_conflict","key":"lease/b","expected_version":"1","current_version":"2","index":"1"})
    );
    for original in [a, target] {
        assert_eq!(
            runtime
                .notes(&token)
                .unwrap()
                .get_note(original.id)
                .await
                .unwrap()
                .unwrap(),
            original
        );
    }
    let renewed = runtime
        .notes(&token)
        .unwrap()
        .get_note(b.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(renewed.version, 2);
    assert_eq!(renewed.content, "{\"renewed\":true}");
}
