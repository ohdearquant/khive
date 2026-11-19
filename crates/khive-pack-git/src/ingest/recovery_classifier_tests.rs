use super::*;

fn err(phase: GitLogPhase, stderr: &str) -> GitLogError {
    GitLogError {
        phase,
        stderr: stderr.to_string(),
    }
}

const REAL_WORLD_MESSAGE: &str = "fatal: deadbeefdeadbeefdeadbeefdeadbeefdeadbeef is in \
         the commit graph file, but not in the object database\nfatal: unable to parse commit: \
         deadbeefdeadbeefdeadbeefdeadbeefdeadbeef\nfatal: could not fetch from promisor remote";

#[test]
fn classifies_real_world_missing_promisor_object_message_on_either_phase() {
    assert!(err(GitLogPhase::TouchedFiles, REAL_WORLD_MESSAGE).is_missing_promisor_object());
    assert!(err(GitLogPhase::Metadata, REAL_WORLD_MESSAGE).is_missing_promisor_object());
}

#[test]
fn classifies_missing_object_wording_case_insensitively() {
    assert!(err(
        GitLogPhase::TouchedFiles,
        "FATAL: MISSING OBJECT abc123; PROMISOR remote unavailable"
    )
    .is_missing_promisor_object());
}

#[test]
fn does_not_classify_bad_object_without_promisor() {
    assert!(!err(GitLogPhase::Metadata, "fatal: bad object HEAD").is_missing_promisor_object());
}

#[test]
fn does_not_classify_auth_or_network_failures() {
    assert!(!err(
        GitLogPhase::Metadata,
        "fatal: Authentication failed for 'https://example.com/org/repo.git/'"
    )
    .is_missing_promisor_object());
    assert!(!err(
        GitLogPhase::TouchedFiles,
        "fatal: unable to access 'https://example.com/org/repo.git/': Could not resolve host"
    )
    .is_missing_promisor_object());
}

#[test]
fn does_not_classify_promisor_mention_without_missing_object_wording() {
    // "promisor" alone (e.g. a config-dump or unrelated log line) must
    // not be treated as proof of corruption -- both keyword classes are
    // required.
    assert!(!err(
        GitLogPhase::Metadata,
        "fatal: promisor remote configured but unreachable"
    )
    .is_missing_promisor_object());
}

/// Healthy repo: loads on first try, no recover call, no warning. Holds
/// `cache::ENV_MUTEX` — see crates/khive-pack-git/docs/api/ingest.md#test-module-notes.
#[test]
fn recover_commit_snapshot_returns_no_warning_when_healthy() {
    let _env = crate::cache::ENV_MUTEX.blocking_lock();
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo_with_commit(dir.path());
    let mut recover_calls = 0;
    let (snapshot, warning) =
        recover_commit_snapshot(dir.path(), None, None, None, None, |_repo, _err| {
            recover_calls += 1;
            Ok(None)
        })
        .expect("healthy repo loads");
    assert_eq!(snapshot.commits.len(), 1);
    assert_eq!(warning, None);
    assert_eq!(recover_calls, 0);
}

/// An unclassified `git log` failure must never reach `recover` and
/// must propagate as-is. Same `ENV_MUTEX` requirement as above.
#[test]
fn recover_commit_snapshot_never_calls_recover_for_unclassified_failures() {
    let _env = crate::cache::ENV_MUTEX.blocking_lock();
    let dir = tempfile::tempdir().expect("tempdir");
    // Not a git repo at all -- `git log` fails with a plain spawn/repo
    // error, not a classified promisor one.
    let mut recover_calls = 0;
    let result = recover_commit_snapshot(dir.path(), None, None, None, None, |_repo, _err| {
        recover_calls += 1;
        Ok(Some(RecoveredRepo {
            repo: dir.path().to_path_buf(),
            strategy: CacheRepairStrategy::Refetch,
        }))
    });
    assert!(result.is_err(), "a non-repo path must fail to load");
    assert_eq!(
        recover_calls, 0,
        "an unclassified failure must never invoke recover"
    );
}

fn init_repo_with_commit(repo: &Path) {
    let run = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    run(&["init", "-q", "-b", "main"]);
    run(&["config", "user.email", "test@example.com"]);
    run(&["config", "user.name", "Test User"]);
    std::fs::write(repo.join("a.txt"), b"hello").unwrap();
    run(&["add", "a.txt"]);
    run(&["commit", "-q", "-m", "initial"]);
}
