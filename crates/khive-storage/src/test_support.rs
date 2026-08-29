//! Shared test-only fixtures for storage-adjacent crates.
//!
//! Gated behind the `test-support` feature so it never ships in a default
//! build; consumers pull it in as a dev-dependency.

/// Freeze any lingering `-wal`/`-shm` sidecars for `path` by making them
/// read-only. Fixtures that close their SQLite connection asynchronously can
/// leave a writable sidecar behind; read-only admission rejects a writable
/// `-shm` as potentially live, so tests that reopen the file read-only freeze
/// the sidecars first to land on the documented frozen-snapshot form.
#[cfg(unix)]
pub fn freeze_snapshot_sidecars(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    for suffix in ["-wal", "-shm"] {
        let mut name = path.file_name().expect("db file name").to_os_string();
        name.push(suffix);
        let sidecar = path.parent().expect("db parent dir").join(name);
        if sidecar.exists() {
            let mut permissions = std::fs::metadata(&sidecar)
                .expect("sidecar metadata")
                .permissions();
            permissions.set_mode(0o444);
            std::fs::set_permissions(&sidecar, permissions).expect("freeze sidecar");
        }
    }
}
