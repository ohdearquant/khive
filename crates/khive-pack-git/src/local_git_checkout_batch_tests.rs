use super::super::{base_command, checkout, oid_output, parse_listing, TreeEntry};
use super::*;
use khive_runtime::{KhiveRuntime, RuntimeConfig};
use khive_storage::{BlobStore, ContentRef, StorageCapability, StorageError, StorageResult};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::AtomicUsize;

#[derive(Debug)]
struct ObservedStore {
    inner: khive_db::stores::blob::FsBlobStore,
    puts: AtomicUsize,
    refuse_put: AtomicBool,
    blocked_put: Mutex<Option<Arc<PutGate>>>,
}

#[derive(Debug)]
struct PutGate {
    entered: tokio::sync::Notify,
    release: tokio::sync::Semaphore,
}

#[async_trait::async_trait]
impl BlobStore for ObservedStore {
    async fn put(&self, bytes: Vec<u8>) -> StorageResult<ContentRef> {
        self.puts.fetch_add(1, Ordering::SeqCst);
        if self.refuse_put.load(Ordering::SeqCst) {
            return Err(StorageError::driver(
                StorageCapability::Blob,
                "put",
                std::io::Error::other("fixture refuses a premature blob write"),
            ));
        }
        let gate = self.blocked_put.lock().unwrap().take();
        if let Some(gate) = gate {
            gate.entered.notify_one();
            gate.release.acquire().await.unwrap().forget();
        }
        self.inner.put(bytes).await
    }

    async fn get_bounded_verified(
        &self,
        reference: &ContentRef,
        limit: u64,
    ) -> StorageResult<Vec<u8>> {
        self.inner.get_bounded_verified(reference, limit).await
    }

    async fn exists(&self, reference: &ContentRef) -> StorageResult<bool> {
        self.inner.exists(reference).await
    }

    async fn size(&self, reference: &ContentRef) -> StorageResult<Option<u64>> {
        self.inner.size(reference).await
    }

    async fn delete(&self, reference: &ContentRef) -> StorageResult<bool> {
        self.inner.delete(reference).await
    }
}

struct Fixture {
    _directory: tempfile::TempDir,
    repo: PathBuf,
    runtime: Arc<KhiveRuntime>,
    store: Arc<ObservedStore>,
}

impl Fixture {
    fn new(files: &[(&str, &[u8], u32)]) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let repo = directory.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let git = |argv: &[&str]| {
            let output = base_command(Path::new("git"))
                .arg("-C")
                .arg(&repo)
                .args(argv)
                .output()
                .unwrap();
            assert!(output.status.success(), "fixture git failed");
        };
        git(&["init", "--quiet", "--template="]);
        std::fs::create_dir_all(repo.join(".git/info")).unwrap();
        std::fs::write(
            repo.join(".git/info/exclude"),
            "trace\nlisting\nbatch-mode\nchild-pid\nrequests\n",
        )
        .unwrap();
        for (name, bytes, mode) in files {
            let path = repo.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, bytes).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(*mode)).unwrap();
        }
        git(&["add", "--all"]);
        git(&[
            "-c",
            "user.name=fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "fixture",
        ]);
        let wrapper = directory.path().join("git-wrapper");
        std::fs::write(&wrapper, concat!(
            "#!/bin/sh\nrepo=\nstate=0\nop=\nbatch=0\n",
            "for arg do\n",
            " if [ \"$state\" = 1 ]; then repo=\"$arg\"; state=2; continue; fi\n",
            " if [ \"$state\" = 2 ]; then op=\"$arg\"; state=3; fi\n",
            " if [ \"$arg\" = -C ]; then state=1; fi\n",
            " if [ \"$arg\" = --batch ]; then batch=1; fi\n",
            "done\nprintf '%s\\n' \"$*\" >> \"$repo/trace\"\n",
            "if [ \"$op\" = ls-tree ] && [ -f \"$repo/listing\" ]; then cat \"$repo/listing\"; exit 0; fi\n",
            "if [ \"$batch\" = 1 ] && [ -f \"$repo/batch-mode\" ]; then\n",
            " mode=$(cat \"$repo/batch-mode\")\n",
            " case \"$mode\" in\n",
            " blocked) printf '%s\\n' \"$$\" > \"$repo/child-pid\"; exec sleep 300;;\n",
            " truncated-closed-stdout) read oid; printf '%s blob 3\\na' \"$oid\"; exec 1>&-; printf '%s\\n' \"$$\" > \"$repo/child-pid\"; exec sleep 300;;\n",
            " bad-separator) read oid; printf '%s blob 3\\nabcz' \"$oid\"; exit 0;;\n",
            " bad-separator-blocked) printf '%s\\n' \"$$\" > \"$repo/child-pid\"; read oid; printf '%s blob 3\\nabcz' \"$oid\"; exec sleep 300;;\n",
            " oversized) read oid; printf '%s blob 67108865\\n' \"$oid\"; exit 0;;\n",
            " stderr-flood) dd if=/dev/zero bs=65536 count=4 >&2 2>/dev/null; while read oid; do printf '%s blob 1\\nx\\n' \"$oid\"; done; exit 0;;\n",
            " requests) while read oid; do printf 'request\\n' >> \"$repo/requests\"; printf '%s blob 1\\nx\\n' \"$oid\"; done; exit 0;;\n",
            " esac\nfi\nexec git \"$@\"\n"
        )).unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut config = RuntimeConfig::no_embeddings();
        config.db_path = Some(directory.path().join("runtime.db"));
        config.git_write.program = Some(wrapper);
        let runtime = Arc::new(KhiveRuntime::new(config).unwrap());
        let store = Arc::new(ObservedStore {
            inner: khive_db::stores::blob::FsBlobStore::new(directory.path().join("blobs"), 0)
                .unwrap(),
            puts: AtomicUsize::new(0),
            refuse_put: AtomicBool::new(false),
            blocked_put: Mutex::new(None),
        });
        runtime.install_blob_store(store.clone()).unwrap();
        Self {
            _directory: directory,
            repo,
            runtime,
            store,
        }
    }

    fn git(&self, argv: &[&str], input: Option<&[u8]>) -> Vec<u8> {
        let mut command = base_command(Path::new("git"));
        command
            .arg("-C")
            .arg(&self.repo)
            .args(argv)
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().unwrap();
        if let Some(bytes) = input {
            child.stdin.take().unwrap().write_all(bytes).unwrap();
        }
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "fixture git failed");
        output.stdout
    }

    fn oid(&self, reference: &str) -> String {
        oid_output(&self.git(&["rev-parse", reference], None)).unwrap()
    }

    fn trace_count(&self, command: &str) -> usize {
        std::fs::read_to_string(self.repo.join("trace"))
            .unwrap_or_default()
            .lines()
            .filter(|line| line.contains(command))
            .count()
    }

    fn mode(&self, mode: &str) {
        std::fs::write(self.repo.join("batch-mode"), mode).unwrap();
    }

    fn listing(&self, oid: &str) {
        std::fs::write(self.repo.join("listing"), format!("100644 blob {oid}\ta\0")).unwrap();
    }
}

#[tokio::test]
async fn checkout_uses_one_batch_child_and_preserves_native_blob_bytes() {
    for population in [0, 1, 16, 64] {
        let paths = (0..population)
            .map(|index| format!("d/file-{index:04}\t\n界"))
            .collect::<Vec<_>>();
        let contents: [&[u8]; 3] = [b"", b"x\0\n\xff", b"repeat\n"];
        let files = paths
            .iter()
            .enumerate()
            .map(|(index, path)| {
                (
                    path.as_str(),
                    contents[index % 3],
                    if index % 2 == 0 { 0o644 } else { 0o755 },
                )
            })
            .collect::<Vec<_>>();
        let fixture = Fixture::new(&files);
        fixture.git(
            &["config", "filter.fixture.clean", "hostile-filter-command"],
            None,
        );
        fixture.git(&["config", "filter.fixture.required", "true"], None);
        let status_before = fixture.git(&["status", "--porcelain=v1", "-z"], None);
        let index_before = std::fs::read(fixture.repo.join(".git/index")).unwrap();
        let result = checkout(&fixture.runtime, &fixture.repo, "HEAD")
            .await
            .unwrap();
        assert_eq!(result.commit, fixture.oid("HEAD"));
        let entries = tree::load(&fixture.runtime, &result.tree).await.unwrap();
        assert_eq!(entries.len(), population);
        for entry in entries {
            let reference = ContentRef::from_hex(&entry.content_ref).unwrap();
            let actual = fixture
                .store
                .get_bounded_verified(&reference, MAX_BLOB_WHOLE_BYTES)
                .await
                .unwrap();
            let native = fixture.git(&["cat-file", "blob", &format!("HEAD:{}", entry.path)], None);
            assert_eq!(actual, native, "checkout must preserve native blob bytes");
            let index = paths.iter().position(|path| *path == entry.path).unwrap();
            assert_eq!(entry.mode, if index % 2 == 0 { 644 } else { 755 });
        }
        assert_eq!(
            fixture.trace_count("cat-file --batch"),
            usize::from(population != 0),
            "checkout must start exactly one batch child for a nonempty tree"
        );
        assert_eq!(
            fixture.trace_count("cat-file blob"),
            0,
            "ordinary blobs must not start per-entry children"
        );
        assert_eq!(
            fixture.trace_count("config -z"),
            1,
            "every child must retain the same per-operation filter snapshot"
        );
        if population != 0 {
            let trace = std::fs::read_to_string(fixture.repo.join("trace")).unwrap();
            let batch = trace
                .lines()
                .find(|line| line.contains("cat-file --batch"))
                .unwrap();
            for expected in super::super::HARDENING.iter().copied().chain([
                "filter.fixture.clean=",
                "filter.fixture.smudge=",
                "filter.fixture.process=",
                "filter.fixture.required=false",
            ]) {
                assert!(
                    batch.contains(&format!("-c {expected} ")),
                    "the batch child must retain each hardening and filter override"
                );
            }
        }
        assert_eq!(
            fixture.git(&["status", "--porcelain=v1", "-z"], None),
            status_before
        );
        assert_eq!(
            std::fs::read(fixture.repo.join(".git/index")).unwrap(),
            index_before
        );
    }
}

#[tokio::test]
async fn checkout_bounds_each_blob_without_rejecting_the_aggregate() {
    let a = vec![b'a'; 33 * 1024 * 1024];
    let b = vec![b'b'; 33 * 1024 * 1024];
    let fixture = Fixture::new(&[("a", &a, 0o644), ("b", &b, 0o755)]);
    let result = checkout(&fixture.runtime, &fixture.repo, "HEAD").await;
    assert!(
        result.is_ok(),
        "multiple legal blobs may exceed the whole-blob limit in aggregate"
    );
    let entries = tree::load(&fixture.runtime, &result.unwrap().tree)
        .await
        .unwrap();
    assert_eq!(entries.len(), 2);
    for (entry, expected) in entries.iter().zip([&a, &b]) {
        let reference = ContentRef::from_hex(&entry.content_ref).unwrap();
        assert_eq!(
            fixture
                .store
                .get_bounded_verified(&reference, MAX_BLOB_WHOLE_BYTES)
                .await
                .unwrap(),
            *expected
        );
    }
    fixture.mode("oversized");
    let before = fixture.store.puts.load(Ordering::SeqCst);
    let error = checkout(&fixture.runtime, &fixture.repo, "HEAD")
        .await
        .unwrap_err();
    assert_eq!(
        error.code(),
        "output_limit",
        "one oversized frame must refuse before body allocation"
    );
    assert_eq!(fixture.store.puts.load(Ordering::SeqCst), before);
}

#[tokio::test]
async fn checkout_rejects_oversized_manifest_before_blob_reads_or_writes() {
    let fixture = Fixture::new(&[("a", b"x", 0o644)]);
    let oid = fixture.oid("HEAD:a");
    let mut listing = Vec::new();
    for index in 0..60000 {
        listing.extend_from_slice(
            format!("100644 blob {oid}\td/{index:06}/{}\0", "x".repeat(80)).as_bytes(),
        );
    }
    std::fs::write(fixture.repo.join("listing"), listing).unwrap();
    fixture.store.refuse_put.store(true, Ordering::SeqCst);
    let error = checkout(&fixture.runtime, &fixture.repo, "HEAD")
        .await
        .unwrap_err();
    assert_eq!(
        fixture.store.puts.load(Ordering::SeqCst),
        0,
        "manifest admission must precede every blob write"
    );
    assert_eq!(
        fixture.trace_count("cat-file"),
        0,
        "manifest admission must precede the blob child"
    );
    assert_eq!(error.code(), "output_limit");
}

#[test]
fn manifest_preflight_counts_actual_json_escaping_and_fixed_digest_width() {
    for path in ["a", "tab\tline\nquote\"界", "directory/a"] {
        let listed = vec![ListedBlob {
            path: path.into(),
            oid: "0".repeat(40),
            mode: 755,
        }];
        let preview = PreviewManifest {
            schema: "khive-tree/v1",
            entries: vec![PreviewEntry {
                path,
                content_ref: PLACEHOLDER_REF,
                mode: 755,
            }],
        };
        let actual = serde_json::json!({"schema":"khive-tree/v1", "entries":tree::entries_json(&[
            TreeEntry { path: path.into(), content_ref: "ab".repeat(32), mode: 755 }
        ])});
        assert_eq!(
            serde_json::to_vec(&preview).unwrap().len(),
            actual.to_string().len()
        );
        admit_manifest(&listed).unwrap();
    }
    let listed = vec![ListedBlob {
        path: "\n".repeat(tree::MAX_MANIFEST_BYTES as usize / 2),
        oid: "0".repeat(40),
        mode: 644,
    }];
    let admission = admit_manifest(&listed);
    assert!(
        matches!(admission, Err(ref error) if error.code() == "output_limit"),
        "JSON escapes count toward the manifest limit"
    );
}

#[tokio::test]
async fn batch_fallback_preserves_missing_and_nonblob_native_refusals() {
    let fixture = Fixture::new(&[("a", b"x", 0o644)]);
    for oid in ["0".repeat(40), fixture.oid("HEAD")] {
        fixture.listing(&oid);
        let native = super::super::run_async(
            fixture.runtime.config().git_write.git_program(),
            &fixture.repo,
            &["cat-file", "blob", &oid],
            None,
        )
        .await
        .unwrap_err();
        let before = fixture.store.puts.load(Ordering::SeqCst);
        let error = checkout(&fixture.runtime, &fixture.repo, "HEAD")
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            native.to_string(),
            "batch in-band refusal must retain the observed native exit status"
        );
        assert_eq!(fixture.store.puts.load(Ordering::SeqCst), before);
    }
}

#[tokio::test]
async fn truncated_loose_blob_preserves_the_native_failure_before_any_put() {
    let mut state = 0x1234_5678_9abc_def0_u64;
    let bytes = (0..200_000)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect::<Vec<_>>();
    let fixture = Fixture::new(&[("a", &bytes, 0o644)]);
    let oid = fixture.oid("HEAD:a");
    let object = fixture
        .repo
        .join(".git/objects")
        .join(&oid[..2])
        .join(&oid[2..]);
    let original_size = std::fs::metadata(&object).unwrap().len();
    assert!(
        original_size > 128 * 1024,
        "fixture must stream a real loose blob"
    );
    std::fs::set_permissions(&object, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&object)
        .unwrap()
        .set_len(original_size / 2)
        .unwrap();
    let native = super::super::run_async(
        fixture.runtime.config().git_write.git_program(),
        &fixture.repo,
        &["cat-file", "blob", &oid],
        None,
    )
    .await
    .unwrap_err();
    assert_eq!(native.code(), "git_failed");
    assert!(native.to_string().ends_with("exit status 128"));
    let mut batch = base_command(Path::new("git"))
        .arg("-C")
        .arg(&fixture.repo)
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    batch
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{oid}\n").as_bytes())
        .unwrap();
    let batch = batch.wait_with_output().unwrap();
    let header = format!("{oid} blob {}\n", bytes.len());
    assert_eq!(batch.status.code(), Some(128));
    assert!(
        batch.stdout.starts_with(header.as_bytes())
            && batch.stdout.len() > header.len()
            && batch.stdout.len() < header.len() + bytes.len(),
        "fixture must fail after a valid native batch header and partial body"
    );
    let before = fixture.store.puts.load(Ordering::SeqCst);
    let error = checkout(&fixture.runtime, &fixture.repo, "HEAD")
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        native.to_string(),
        "a truncated native object stream must preserve the original exit-status refusal"
    );
    assert_eq!(
        fixture.store.puts.load(Ordering::SeqCst),
        before,
        "a failed native object stream must refuse before writing its blob"
    );
}

#[tokio::test]
async fn batch_fallback_preserves_tag_to_blob_success_and_the_next_frame() {
    let fixture = Fixture::new(&[("a", b"x\0\n", 0o644), ("b", b"second", 0o755)]);
    let blob = fixture.oid("HEAD:a");
    let tag = fixture.git(&["mktag"], Some(format!(
        "object {blob}\ntype blob\ntag blob-tag\ntagger fixture <fixture@example.invalid> 1 +0000\n\nfixture\n"
    ).as_bytes()));
    let tag = oid_output(&tag).unwrap();
    let second = fixture.oid("HEAD:b");
    std::fs::write(
        fixture.repo.join("listing"),
        format!(
            "100644 blob {tag}\ta\0{mode} blob {second}\tb\0",
            mode = "100755"
        ),
    )
    .unwrap();
    let result = checkout(&fixture.runtime, &fixture.repo, "HEAD").await;
    assert!(
        result.is_ok(),
        "tag-to-blob fallback must preserve native success and drain its batch frame"
    );
    let entries = tree::load(&fixture.runtime, &result.unwrap().tree)
        .await
        .unwrap();
    assert_eq!(entries.len(), 2);
    for entry in &entries {
        let reference = ContentRef::from_hex(&entry.content_ref).unwrap();
        let expected: &[u8] = if entry.path == "a" {
            b"x\0\n"
        } else {
            b"second"
        };
        assert_eq!(
            fixture
                .store
                .get_bounded_verified(&reference, MAX_BLOB_WHOLE_BYTES)
                .await
                .unwrap(),
            expected
        );
    }
    assert_eq!(fixture.trace_count("cat-file --batch"), 1);
    assert_eq!(
        fixture.trace_count("cat-file blob"),
        1,
        "only the exceptional tag dereference may start a fallback child"
    );
}

#[test]
fn batch_protocol_checks_headers_sizes_ids_and_body_separators() {
    let oid = "1".repeat(40);
    for header in [
        format!("{} blob 1\n", "2".repeat(40)),
        format!("{oid} blob -1\n"),
        format!("{oid} blob 18446744073709551616\n"),
        format!("{oid} unknown 1\n"),
        format!("{oid} {}\n", "x".repeat(150)),
        format!("{oid} blob 1"),
    ] {
        let result = read_header(&mut std::io::Cursor::new(header), &oid);
        assert!(result.is_err(), "malformed batch header must refuse");
    }
    for body in [&b"abcz"[..], &b"ab"[..]] {
        let result = read_blob(&mut &body[..], 3);
        assert!(
            matches!(result, Err(ref error) if error.code() == "git_output"),
            "truncated bodies and non-LF separators must refuse"
        );
    }
    assert_eq!(read_blob(&mut &b"\0\n\xff\n"[..], 3).unwrap(), b"\0\n\xff");
    assert_eq!(read_blob(&mut &b"\n"[..], 0).unwrap(), b"");
}

#[tokio::test]
async fn batch_worker_waits_for_storage_before_requesting_another_blob() {
    let fixture = Fixture::new(&[("a", b"x", 0o644)]);
    fixture.mode("requests");
    let oid = fixture.oid("HEAD:a");
    let listed = parse_listing(
        format!(
            "100644 blob {oid}\ta\0{mode} blob {oid}\tb\0",
            mode = "100644"
        )
        .as_bytes(),
    )
    .unwrap();
    let mut filters = None;
    let mut reader = BlobReader::start(
        fixture.runtime.config().git_write.git_program(),
        &fixture.repo,
        &listed,
        &mut filters,
    )
    .await
    .unwrap();
    let first = reader.next().await.unwrap();
    let early = tokio::time::timeout(Duration::from_secs(1), reader.next()).await;
    let advanced = early.is_ok();
    let requests = std::fs::read_to_string(fixture.repo.join("requests"))
        .unwrap()
        .lines()
        .count();
    let _ = first.consumed.send(());
    let second = match early {
        Ok(frame) => frame.unwrap(),
        Err(_) => reader.next().await.unwrap(),
    };
    let _ = second.consumed.send(());
    reader.finish().await.unwrap();
    assert!(
        !advanced,
        "storage acknowledgement must precede the next blob frame"
    );
    assert_eq!(
        requests, 1,
        "the worker must not retain or request a second body before storage completes"
    );
}

#[tokio::test]
async fn batch_child_drains_stderr_beyond_its_retained_limit() {
    let fixture = Fixture::new(&[("a", b"x", 0o644)]);
    fixture.mode("stderr-flood");
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        checkout(&fixture.runtime, &fixture.repo, "HEAD"),
    )
    .await;
    assert!(
        matches!(result, Ok(Ok(_))),
        "large stderr must be drained without blocking or refusing a legal blob"
    );
}

fn process_alive(pid: &str) -> bool {
    Command::new("kill")
        .args(["-0", pid])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success()
}

async fn child_pid(fixture: &Fixture) -> String {
    for _ in 0..500 {
        if let Ok(pid) = std::fs::read_to_string(fixture.repo.join("child-pid")) {
            return pid.trim().to_string();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("fixture child did not start");
}

#[tokio::test]
async fn cancelling_checkout_kills_and_reaps_a_blocked_batch_child() {
    for mode in ["blocked", "truncated-closed-stdout"] {
        let fixture = Fixture::new(&[("a", b"x", 0o644)]);
        fixture.mode(mode);
        let runtime = Arc::clone(&fixture.runtime);
        let repo = fixture.repo.clone();
        let task = tokio::spawn(async move { checkout(&runtime, &repo, "HEAD").await });
        let pid = child_pid(&fixture).await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        for _ in 0..500 {
            if !process_alive(&pid) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let alive = process_alive(&pid);
        if alive {
            let _ = Command::new("kill").args(["-KILL", &pid]).status();
        }
        assert!(
            !alive,
            "cancelled checkout must kill and reap the batch child"
        );
    }
}

#[tokio::test]
async fn malformed_batch_output_kills_a_child_that_keeps_its_pipes_open() {
    let fixture = Fixture::new(&[("a", b"x", 0o644)]);
    fixture.mode("bad-separator-blocked");
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        checkout(&fixture.runtime, &fixture.repo, "HEAD"),
    )
    .await;
    let pid = child_pid(&fixture).await;
    let completed = result.is_ok();
    let alive = process_alive(&pid);
    if alive {
        let _ = Command::new("kill").args(["-KILL", &pid]).status();
    }
    assert!(
        completed,
        "a protocol refusal must reap its child and complete without waiting for pipe closure"
    );
    assert_eq!(result.unwrap().unwrap_err().code(), "git_output");
    assert!(!alive, "the malformed-output child must already be reaped");
}

#[tokio::test]
async fn checkout_stores_the_previous_blob_before_acknowledging_its_frame() {
    let fixture = Fixture::new(&[("a", b"x", 0o644), ("b", b"x", 0o755)]);
    fixture.mode("requests");
    let gate = Arc::new(PutGate {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Semaphore::new(0),
    });
    *fixture.store.blocked_put.lock().unwrap() = Some(Arc::clone(&gate));
    let runtime = Arc::clone(&fixture.runtime);
    let repo = fixture.repo.clone();
    let mut task = tokio::spawn(async move { checkout(&runtime, &repo, "HEAD").await });
    let entered = tokio::time::timeout(Duration::from_secs(5), gate.entered.notified()).await;
    if entered.is_err() {
        task.abort();
        let _ = task.await;
        panic!("checkout never reached the blocking BlobStore put");
    }
    let puts_while_blocked = fixture.store.puts.load(Ordering::SeqCst);
    let reference = ContentRef::from_digest_bytes(blake3::hash(b"x").as_bytes());
    let stored_while_blocked = fixture.store.exists(&reference).await.unwrap();
    let mut requests_while_blocked = 0;
    for _ in 0..100 {
        requests_while_blocked = std::fs::read_to_string(fixture.repo.join("requests"))
            .unwrap()
            .lines()
            .count();
        if requests_while_blocked > 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    gate.release.add_permits(1);
    let completed = tokio::time::timeout(Duration::from_secs(5), &mut task).await;
    if completed.is_err() {
        task.abort();
        let _ = task.await;
        panic!("checkout did not complete after releasing BlobStore put");
    }
    let checkout = completed.unwrap().unwrap().unwrap();
    let entries = tree::load(&fixture.runtime, &checkout.tree).await.unwrap();
    assert_eq!(entries.len(), 2);
    for entry in entries {
        let reference = ContentRef::from_hex(entry.content_ref).unwrap();
        assert_eq!(
            fixture
                .store
                .get_bounded_verified(&reference, 1)
                .await
                .unwrap(),
            b"x"
        );
    }
    let requests_after_release = std::fs::read_to_string(fixture.repo.join("requests"))
        .unwrap()
        .lines()
        .count();
    assert_eq!(
        requests_after_release, 2,
        "both real checkout entries must complete"
    );
    assert_eq!(puts_while_blocked, 1);
    assert!(
        !stored_while_blocked,
        "the first put must still be incomplete"
    );
    assert_eq!(
        requests_while_blocked, 1,
        "checkout must not acknowledge a frame before its BlobStore put completes"
    );
}

#[test]
fn batch_header_bound_refuses_a_valid_shape_with_a_parseable_digit_size() {
    let oid = "1".repeat(40);
    let size = format!("{}1", "0".repeat(100));
    assert!(size.bytes().all(|byte| byte.is_ascii_digit()));
    assert_eq!(size.parse::<u64>().unwrap(), 1);
    let header = format!("{oid} blob {size}\n");
    assert!(header.len() as u64 > HEADER_LIMIT);
    assert_eq!(header.trim_end().split(' ').count(), 3);
    let result = read_header(&mut std::io::Cursor::new(header), &oid);
    assert!(
        matches!(result, Err(ref error) if error.code() == "git_output"
            && error.to_string() == "git_output: invalid batch header length"),
        "a valid-shaped, parseable header must still obey the byte bound"
    );
}

#[tokio::test]
async fn oversized_truncated_loose_blob_prioritizes_the_declared_size_limit() {
    let mut state = 0x1234_5678_9abc_def0_u64;
    let bytes = (0..MAX_BLOB_WHOLE_BYTES as usize + 1)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect::<Vec<_>>();
    let fixture = Fixture::new(&[("a", &bytes, 0o644)]);
    let oid = fixture.oid("HEAD:a");
    let object = fixture
        .repo
        .join(".git/objects")
        .join(&oid[..2])
        .join(&oid[2..]);
    let original_size = std::fs::metadata(&object).unwrap().len();
    assert!(original_size > MAX_BLOB_WHOLE_BYTES);
    std::fs::set_permissions(&object, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&object)
        .unwrap()
        .set_len(original_size / 2)
        .unwrap();
    let native = super::super::run_async(
        fixture.runtime.config().git_write.git_program(),
        &fixture.repo,
        &["cat-file", "blob", &oid],
        None,
    )
    .await
    .unwrap_err();
    assert_eq!(native.code(), "git_failed");
    assert!(native.to_string().ends_with("exit status 128"));
    let mut batch = base_command(Path::new("git"))
        .arg("-C")
        .arg(&fixture.repo)
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    batch
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{oid}\n").as_bytes())
        .unwrap();
    let batch = batch.wait_with_output().unwrap();
    let header = format!("{oid} blob {}\n", bytes.len());
    assert_eq!(batch.status.code(), Some(128));
    assert!(
        batch.stdout.starts_with(header.as_bytes())
            && batch.stdout.len() > header.len()
            && batch.stdout.len() < header.len() + bytes.len(),
        "fixture must expose an oversized native header before a truncated body"
    );
    let before = fixture.store.puts.load(Ordering::SeqCst);
    let error = checkout(&fixture.runtime, &fixture.repo, "HEAD")
        .await
        .unwrap_err();
    assert_eq!(
        error.code(),
        "output_limit",
        "the declared oversized frame must take priority over its later native truncation"
    );
    assert_eq!(
        error.to_string(),
        "output_limit: git output exceeds the whole-blob limit"
    );
    assert_eq!(fixture.store.puts.load(Ordering::SeqCst), before);
}
