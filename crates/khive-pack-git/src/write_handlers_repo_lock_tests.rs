use super::*;

fn registered(key: &Path) -> bool {
    REPO_LOCKS.get().unwrap().lock().unwrap().contains_key(key)
}

#[test]
fn final_repo_handle_releases_only_its_registry_entry() {
    let temp = tempfile::tempdir().unwrap();
    let key = std::fs::canonicalize(temp.path()).unwrap();
    let first = repo_write_lock(&key);
    let second = repo_write_lock(&key);
    assert!(Arc::ptr_eq(
        first.lock.as_ref().unwrap(),
        second.lock.as_ref().unwrap()
    ));
    drop(first);
    assert!(registered(&key), "another handle still names this lock");
    drop(second);
    assert!(!registered(&key), "the unused entry must be removed");
}

#[cfg(unix)]
#[tokio::test]
async fn canonical_repo_aliases_share_the_active_lock() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    let alias = temp.path().join("alias");
    std::fs::create_dir(&repo).unwrap();
    std::os::unix::fs::symlink(&repo, &alias).unwrap();
    let first = repo_write_lock(&repo);
    let guard = first.lock().await;
    let second = repo_write_lock(&alias);
    assert!(Arc::ptr_eq(
        first.lock.as_ref().unwrap(),
        second.lock.as_ref().unwrap()
    ));
    assert!(second.try_lock().is_err());
    drop(guard);
    assert!(second.try_lock().is_ok());
}

#[tokio::test]
async fn distinct_repo_locks_do_not_block_each_other() {
    let first_repo = tempfile::tempdir().unwrap();
    let second_repo = tempfile::tempdir().unwrap();
    let first = repo_write_lock(first_repo.path());
    let _guard = first.lock().await;
    let second = repo_write_lock(second_repo.path());
    assert!(second.try_lock().is_ok());
}

#[test]
fn concurrent_final_repo_handles_release_registry_entry() {
    let temp = tempfile::tempdir().unwrap();
    let key = std::fs::canonicalize(temp.path()).unwrap();
    for _ in 0..4 {
        let first = repo_write_lock(&key);
        let second = repo_write_lock(&key);
        let start = Arc::new(std::sync::Barrier::new(2));
        let after_unlock = Arc::new(std::sync::Barrier::new(2));
        std::thread::scope(|scope| {
            for handle in [first, second] {
                let start = Arc::clone(&start);
                let after_unlock = Arc::clone(&after_unlock);
                scope.spawn(move || {
                    REPO_DROP_AFTER_UNLOCK.with(|sync| *sync.borrow_mut() = Some(after_unlock));
                    start.wait();
                    drop(handle);
                });
            }
        });
        assert!(
            !registered(&key),
            "concurrent final drops left an unused entry"
        );
    }
}

#[test]
fn registry_capacity_recovers_after_a_repo_burst() {
    let temp = tempfile::tempdir().unwrap();
    let handles: Vec<_> = (0..4096)
        .map(|index| repo_write_lock(&temp.path().join(format!("repo-{index}"))))
        .collect();
    let keys: Vec<_> = handles.iter().map(|handle| handle.key.clone()).collect();
    drop(handles);
    let registry = REPO_LOCKS.get().unwrap().lock().unwrap();
    assert!(keys.iter().all(|key| !registry.contains_key(key)));
    assert!(
        registry.capacity() <= registry.len().saturating_mul(4),
        "capacity {} retained after burst; live entries {}",
        registry.capacity(),
        registry.len(),
    );
}

#[tokio::test]
async fn aborted_waiter_releases_handle_without_replacing_active_lock() {
    let temp = tempfile::tempdir().unwrap();
    let key = std::fs::canonicalize(temp.path()).unwrap();
    let held = repo_write_lock(&key);
    let guard = held.lock().await;
    let ready = Arc::new(tokio::sync::Notify::new());
    let waiter = {
        let handle = repo_write_lock(&key);
        let ready = Arc::clone(&ready);
        tokio::spawn(async move {
            ready.notify_one();
            let _guard = handle.lock().await;
        })
    };
    ready.notified().await;
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    let later = repo_write_lock(&key);
    assert!(later.try_lock().is_err());
    drop(guard);
    assert!(later.try_lock().is_ok());
    drop(held);
    assert!(registered(&key));
    drop(later);
    assert!(!registered(&key));
}

#[tokio::test]
async fn waiting_repo_handle_preserves_lock_identity() {
    let temp = tempfile::tempdir().unwrap();
    let key = std::fs::canonicalize(temp.path()).unwrap();
    let held = repo_write_lock(&key);
    let guard = held.lock().await;
    let waiting = repo_write_lock(&key);
    assert!(waiting.try_lock().is_err());
    drop(waiting);
    let later = repo_write_lock(&key);
    assert!(later.try_lock().is_err(), "held lock must not be replaced");
    drop(guard);
    assert!(later.try_lock().is_ok());
    drop(held);
    assert!(registered(&key));
    drop(later);
    assert!(!registered(&key));
}
