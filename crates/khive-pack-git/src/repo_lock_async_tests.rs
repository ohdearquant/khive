use super::*;

#[tokio::test]
async fn async_repo_aliases_share_lock_and_release_cancelled_waiter() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    let alias = temp.path().join("alias");
    std::fs::create_dir(&repo).unwrap();
    std::os::unix::fs::symlink(&repo, &alias).unwrap();
    let first = repo_write_lock_async(&repo).await.unwrap();
    let guard = first.lock().await;
    let second = repo_write_lock_async(&alias).await.unwrap();
    assert!(Arc::ptr_eq(
        first.lock.as_ref().unwrap(),
        second.lock.as_ref().unwrap()
    ));
    assert!(second.try_lock().is_err());
    let ready = Arc::new(tokio::sync::Notify::new());
    let waiter = {
        let ready = Arc::clone(&ready);
        tokio::spawn(async move {
            ready.notify_one();
            let _guard = second.lock().await;
        })
    };
    ready.notified().await;
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    let later = repo_write_lock_async(&alias).await.unwrap();
    assert!(Arc::ptr_eq(
        first.lock.as_ref().unwrap(),
        later.lock.as_ref().unwrap()
    ));
    assert!(later.try_lock().is_err());
    drop(guard);
    assert!(later.try_lock().is_ok());
    let key = first.key.clone();
    drop(first);
    drop(later);
    assert!(!REPO_LOCKS.get().unwrap().lock().unwrap().contains_key(&key));
}

#[tokio::test]
async fn async_repo_lock_preserves_distinct_paths_and_missing_path_fallback() {
    let temp = tempfile::tempdir().unwrap();
    let first_path = temp.path().join("first-missing");
    let second_path = temp.path().join("second-missing");
    let first = repo_write_lock_async(&first_path).await.unwrap();
    let guard = first.lock().await;
    let repeated = repo_write_lock_async(&first_path).await.unwrap();
    let distinct = repo_write_lock_async(&second_path).await.unwrap();
    assert!(Arc::ptr_eq(
        first.lock.as_ref().unwrap(),
        repeated.lock.as_ref().unwrap()
    ));
    assert!(repeated.try_lock().is_err());
    assert!(distinct.try_lock().is_ok());
    assert_eq!(first.key, first_path);
    assert_eq!(distinct.key, second_path);
    drop(guard);
    assert!(repeated.try_lock().is_ok());
}
