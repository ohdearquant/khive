//! Test-only retention of the rotation watcher task for one `AnnState`, so
//! lifecycle tests can assert on that state's own watcher instead of the
//! process-wide background task counter that unrelated tests also move.

use super::SharedAnn;

/// Keep a freshly started watcher task reachable from the ANN state whose
/// one-shot guard it claimed. A `None` (the guard was already claimed, or the
/// runtime has no file-backed ANN root) leaves any retained task untouched.
pub(super) fn retain_rotation_watch_handle(
    ann: &SharedAnn,
    watcher: Option<tokio::task::JoinHandle<()>>,
) {
    if let Some(watcher) = watcher {
        let handle = &ann.rotation_watch_handle;
        *handle.lock().expect("rotation watch handle lock") = Some(watcher);
    }
}

/// Take the watcher task retained for `ann`. Returns `None` when no watcher
/// was started since the last take, so a second call after a repeated start
/// tells a test whether that start spawned another task.
pub(crate) fn take_rotation_watch_handle_for_test(
    ann: &SharedAnn,
) -> Option<tokio::task::JoinHandle<()>> {
    let handle = &ann.rotation_watch_handle;
    handle.lock().expect("rotation watch handle lock").take()
}
