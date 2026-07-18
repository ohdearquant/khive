//! `code.ingest` target-database selection (ADR-085 Amendment 2 B7).
//!
//! `code.ingest` never writes to the shared production graph: it defaults to
//! a dedicated map database colocated with the ingested path, and rejects an
//! explicit `db` that resolves to the well-known production database path or
//! to the calling runtime's actual configured database, with no override.

use std::path::{Path, PathBuf};

/// The shared production database's default location. Delegates to
/// `khive_runtime::config::resolve_db_anchor(None)` — the SAME resolver
/// `kkernel`/`khive-mcp` use to anchor the production database — rather than
/// re-deriving the fallback here. A prior hand-rolled version of this
/// function only handled `HOME` being SET, returning `None` (no forbidden
/// path at all) when `HOME` was absent, while the canonical resolver falls
/// back to `./.khive/khive.db`; that divergence is exactly what let the
/// fence fail open (#1062 H2). `resolve_db_anchor(None)` always resolves to
/// `Some(_)` (see its own doc comment).
fn default_production_db_path() -> Option<PathBuf> {
    khive_runtime::config::resolve_db_anchor(None)
}

/// Normalize `path` to its deepest *existing* canonical ancestor plus the
/// still-not-yet-created suffix appended back on. This lets two lexically
/// different paths that alias the same file — a symlinked parent directory,
/// or a `db` target whose final file does not exist yet (as is normal for a
/// not-yet-created database) — compare equal, instead of falling back to raw
/// lexical equality the moment either side is missing.
fn normalize(path: &Path) -> PathBuf {
    let mut existing: &Path = path;
    let mut suffix: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if existing.exists() {
            break;
        }
        let Some(name) = existing.file_name() else {
            break;
        };
        suffix.push(name.to_os_string());
        let Some(parent) = existing.parent() else {
            break;
        };
        existing = parent;
    }
    let mut base = existing
        .canonicalize()
        .unwrap_or_else(|_| existing.to_path_buf());
    for part in suffix.into_iter().rev() {
        base.push(part);
    }
    base
}

/// `(device, inode)` for `path` and, when present, its SQLite WAL/SHM
/// companion files — a hard link to a protected database shares the target's
/// inode even though `normalize`'s canonicalized *path* differs (hard links
/// are distinct directory entries pointing at one inode; there is no
/// symlink for `canonicalize` to resolve through). Absent files are simply
/// omitted, not an error: a not-yet-created candidate has no identity to
/// compare here and falls back to `normalize`'s path-alias check instead.
#[cfg(unix)]
fn file_identities(path: &Path) -> Vec<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;

    let mut ids = Vec::new();
    let mut probe = |p: &Path| {
        if let Ok(meta) = std::fs::metadata(p) {
            ids.push((meta.dev(), meta.ino()));
        }
    };
    probe(path);
    for suffix in ["-wal", "-shm"] {
        let mut companion = path.as_os_str().to_os_string();
        companion.push(suffix);
        probe(Path::new(&companion));
    }
    ids
}

/// Windows equivalent of the Unix `(dev, ino)` pair: `(volume_serial_number,
/// file_index)` uniquely identifies a file on NTFS the same way `(dev, ino)`
/// does on Unix, so a hard-linked alias of a protected database is caught
/// here the same way -- a distinct pathname with matching `(volume, index)`
/// still shares one on-disk file.
#[cfg(windows)]
fn file_identities(path: &Path) -> Vec<(u64, u64)> {
    use std::os::windows::fs::MetadataExt;

    let mut ids = Vec::new();
    let mut probe = |p: &Path| {
        if let Ok(meta) = std::fs::metadata(p) {
            if let (Some(vol), Some(idx)) = (meta.volume_serial_number(), meta.file_index()) {
                ids.push((vol as u64, idx));
            }
        }
    };
    probe(path);
    for suffix in ["-wal", "-shm"] {
        let mut companion = path.as_os_str().to_os_string();
        companion.push(suffix);
        probe(Path::new(&companion));
    }
    ids
}

#[cfg(not(any(unix, windows)))]
fn file_identities(_path: &Path) -> Vec<(u64, u64)> {
    Vec::new()
}

/// True when `a` and `b` name the same database file: either the same path
/// once symlinked-parent aliasing is normalized away, or — the hard-link
/// case `normalize` cannot see, since a hard link introduces no symlink for
/// canonicalization to follow — the same `(device, inode)` identity on disk,
/// checked against both files' main path and any present `-wal`/`-shm`
/// companion.
fn same_path(a: &Path, b: &Path) -> bool {
    if normalize(a) == normalize(b) {
        return true;
    }
    let a_ids = file_identities(a);
    if a_ids.is_empty() {
        return false;
    }
    let b_ids = file_identities(b);
    a_ids.iter().any(|id| b_ids.contains(id))
}

/// The production-database paths a target must never alias: the well-known
/// `$HOME/.khive/khive.db` default plus whichever of `runtime_db_path` or
/// `KHIVE_DB` names the actually-configured production database (see
/// `resolve_target_db`'s doc comment for why both are needed).
fn forbidden_db_paths(runtime_db_path: Option<&Path>) -> Vec<PathBuf> {
    let mut forbidden: Vec<PathBuf> = Vec::new();
    if let Some(prod) = default_production_db_path() {
        forbidden.push(prod);
    }
    match runtime_db_path {
        Some(runtime_db) => forbidden.push(runtime_db.to_path_buf()),
        // `config().db_path` is unresolved (not reachable by the production
        // daemon today, which always populates it at startup) — fall back to
        // `KHIVE_DB` directly so the fence is total rather than
        // total-in-practice: an operator running with an env-only override
        // and no resolved config path is still covered (#1042).
        None => {
            if let Ok(env_db) = std::env::var("KHIVE_DB") {
                if !env_db.is_empty() {
                    forbidden.push(PathBuf::from(env_db));
                }
            }
        }
    }
    forbidden
}

fn reject_if_forbidden(candidate: &Path, forbidden: &[PathBuf]) -> Result<(), String> {
    for forbidden_path in forbidden {
        if same_path(candidate, forbidden_path) {
            return Err(format!(
                "code.ingest refuses to target the shared production database ({}); pass \
                 db=<path> pointing at a dedicated map database, or omit db to use the \
                 workspace-local default",
                forbidden_path.display()
            ));
        }
    }
    Ok(())
}

/// True when `raw` is (or could plausibly be interpreted by SQLite as) a
/// `file:` URI target. The connection pool unconditionally sets
/// `SQLITE_OPEN_URI` on every open (`crates/khive-db/src/pool.rs`), so a
/// `db` string SQLite parses as a URI is opened at whatever filesystem path
/// that URI names -- never the literal string a `PathBuf`-based comparison
/// sees. `?`-bearing strings without a `file:` prefix are flagged too, since
/// they read as URI-shaped input even though SQLite itself would not parse
/// them as one; treating them as "needs URI resolution, fail closed if it
/// doesn't parse" is strictly more conservative than passing them through.
fn looks_like_sqlite_uri(raw: &str) -> bool {
    raw.starts_with("file:") || raw.contains('?')
}

/// Decode a percent-encoded UTF-8 string, per the escaping SQLite URIs use
/// for path bytes outside the unreserved set (https://sqlite.org/uri.html).
/// Malformed `%`-escapes are passed through as literal bytes rather than
/// rejected -- callers only use this to derive the path a *successful* parse
/// will compare against the forbidden set, so a caller string that neither
/// parses cleanly nor happens to alias a forbidden target is still safely
/// rejected downstream if it does alias one, and otherwise this is best-effort.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parse a SQLite `file:` URI down to the filesystem path it names. Per
/// https://sqlite.org/uri.html, `file:path`, `file:/path`, and
/// `file://authority/path` (authority ignored) all name a filesystem path;
/// everything from the first `?` or `#` is URI parameters, not path. Returns
/// `None` when `raw` has no `file:` prefix to parse, or decodes to an empty
/// path -- both cases the caller treats as unparseable and fails closed on,
/// rather than falling back to comparing the raw literal (which would let a
/// malformed URI dodge the identity check the way the un-decoded literal did
/// before this fence existed).
fn sqlite_uri_path(raw: &str) -> Option<PathBuf> {
    let rest = raw.strip_prefix("file:")?;
    let rest = rest.split(['?', '#']).next().unwrap_or("");
    let rest = match rest.strip_prefix("//") {
        Some(after_slashes) => match after_slashes.find('/') {
            Some(idx) => &after_slashes[idx..],
            None => after_slashes,
        },
        None => rest,
    };
    if rest.is_empty() {
        return None;
    }
    let mut decoded = percent_decode(rest);
    if has_leading_slash_before_drive_letter(&decoded) {
        decoded.remove(0);
    }
    Some(PathBuf::from(decoded))
}

/// True when `s` is a POSIX-style absolute path whose first segment is a
/// Windows drive letter (`/C:/...`) -- the shape `file:///C:/...` and
/// `file://localhost/C:/...` decode to per RFC 8089, while SQLite's own URI
/// parser strips that leading slash before opening the file, landing on
/// `C:/...` (https://sqlite.org/uri.html). Left unstripped, the fence
/// compares against a path SQLite never actually opens, so a `db` target
/// aimed at the production database under a `file:///C:/...` URI slips past
/// the comparison (#1087 item 3/8).
fn has_leading_slash_before_drive_letter(s: &str) -> bool {
    let bytes = s.as_bytes();
    bytes.len() >= 3 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() && bytes[2] == b':'
}

/// Resolve `raw` to the path the fence must actually check identity
/// against: the URI-decoded target when `raw` is URI-shaped (erroring if it
/// does not parse), otherwise the literal path unchanged.
fn resolve_candidate_for_check(raw: &str) -> Result<PathBuf, String> {
    if looks_like_sqlite_uri(raw) {
        sqlite_uri_path(raw).ok_or_else(|| {
            format!(
                "code.ingest refuses an unparseable SQLite URI db target ({raw:?}); pass a \
                 plain filesystem path, or a well-formed file: URI"
            )
        })
    } else {
        Ok(PathBuf::from(raw))
    }
}

/// Resolve the `db` verb argument into a concrete target database path,
/// defaulting to `<path>/.khive/code-map.db` when absent, and rejecting a
/// target that resolves to the shared production database — either its
/// well-known `$HOME/.khive/khive.db` default, or `runtime_db_path`, the
/// database the calling `KhiveRuntime` was actually constructed against
/// (`self.runtime.config().db_path` at the call site), so an operator running
/// a non-default production location (`--db` / `KHIVE_DB`) is covered too.
pub(crate) fn resolve_target_db(
    db_param: Option<&str>,
    ingest_path: &Path,
    runtime_db_path: Option<&Path>,
) -> Result<PathBuf, String> {
    let (candidate, check_path) = match db_param {
        Some(p) => (PathBuf::from(p), resolve_candidate_for_check(p)?),
        None => {
            let default = ingest_path.join(".khive").join("code-map.db");
            (default.clone(), default)
        }
    };
    reject_if_forbidden(&check_path, &forbidden_db_paths(runtime_db_path))?;
    Ok(candidate)
}

/// Re-verify `opened_db_path` against the same forbidden set immediately
/// after the target `KhiveRuntime` has been opened (or created), before any
/// write runs against it. `resolve_target_db`'s check and the actual open are
/// two separate filesystem operations with a window between them (create a
/// fresh `db_path` after the check observed no existing file, alias it to a
/// protected database in that window); re-stating the now-guaranteed-to-exist
/// path closes most of that window by adding a metadata-identity check the
/// first pass could not perform against a not-yet-created file. This is a
/// narrowing, not a full close — a true race-resistant fence requires a
/// VFS-level check (ADR-085 Amendment 4, in review).
pub(crate) fn verify_opened_target(
    opened_db_path: &Path,
    runtime_db_path: Option<&Path>,
) -> Result<(), String> {
    let check_path = resolve_candidate_for_check(&opened_db_path.to_string_lossy())?;
    reject_if_forbidden(&check_path, &forbidden_db_paths(runtime_db_path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_target_is_workspace_local() {
        let path = Path::new("/tmp/some-repo");
        let db = resolve_target_db(None, path, None).expect("default resolves");
        assert_eq!(db, path.join(".khive").join("code-map.db"));
    }

    #[test]
    fn explicit_production_path_is_rejected() {
        // Reads HOME (does not mutate it), but still takes the shared lock:
        // an unguarded read here can race a concurrently-running test that
        // mutates HOME via `HomeGuard` (cargo test runs tests in the same
        // binary in parallel by default).
        let _guard = KHIVE_DB_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = std::env::var("HOME").expect("HOME set in test env");
        let prod = format!("{home}/.khive/khive.db");
        let err = resolve_target_db(Some(&prod), Path::new("/tmp/some-repo"), None)
            .expect_err("must reject the shared production database");
        assert!(err.contains("shared production database"));
    }

    #[test]
    fn explicit_dedicated_path_is_accepted() {
        let db = resolve_target_db(
            Some("/tmp/code-ingest-map.db"),
            Path::new("/tmp/some-repo"),
            None,
        )
        .expect("dedicated path accepted");
        assert_eq!(db, PathBuf::from("/tmp/code-ingest-map.db"));
    }

    #[test]
    fn nondefault_configured_production_db_is_rejected() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let prod = tmp.path().join("srv-main.db");
        std::fs::write(&prod, b"").expect("create sentinel file");
        let err = resolve_target_db(
            Some(prod.to_str().unwrap()),
            Path::new("/tmp/some-repo"),
            Some(&prod),
        )
        .expect_err("must reject the runtime's actual configured production db");
        assert!(err.contains("shared production database"));
    }

    #[test]
    fn symlinked_parent_alias_of_configured_db_is_rejected_even_before_file_exists() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let real_dir = tmp.path().join("real");
        std::fs::create_dir_all(&real_dir).expect("mkdir");
        let link_dir = tmp.path().join("link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real_dir, &link_dir).expect("symlink");
        #[cfg(not(unix))]
        std::fs::create_dir_all(&link_dir).expect("mkdir fallback");

        let configured = real_dir.join("main.db");
        // Neither the configured db file nor the aliased candidate exists yet
        // — both parents do, and the alias must still be caught.
        let candidate = link_dir.join("main.db");
        let err = resolve_target_db(
            Some(candidate.to_str().unwrap()),
            Path::new("/tmp/some-repo"),
            Some(&configured),
        )
        .expect_err("symlinked-parent alias of the configured db must be rejected");
        assert!(err.contains("shared production database"));
    }

    /// Same hard-link-alias scenario as the Unix test below, exercising the
    /// Windows `(volume_serial_number, file_index)` identity path instead of
    /// `(dev, ino)` -- the CI Windows matrix job (ci.yml `check-windows`)
    /// compiles and runs this.
    #[test]
    #[cfg(windows)]
    fn hard_link_to_configured_db_is_rejected_on_windows() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let configured = tmp.path().join("main.db");
        std::fs::write(&configured, b"").expect("create sentinel file");
        let candidate = tmp.path().join("map.db");
        std::fs::hard_link(&configured, &candidate).expect("hard link");

        let err = resolve_target_db(
            Some(candidate.to_str().unwrap()),
            Path::new("/tmp/some-repo"),
            Some(&configured),
        )
        .expect_err("hard-linked alias of the configured db must be rejected on Windows");
        assert!(err.contains("shared production database"));
    }

    /// #1087 item 1: `SQLITE_OPEN_URI` is set unconditionally by the pool
    /// (`crates/khive-db/src/pool.rs`), so a `db=file:...?...` target is
    /// opened by SQLite at the URI's decoded path, not the literal string a
    /// PathBuf-only comparison would see. The fence must decode the URI
    /// before comparing.
    #[test]
    fn sqlite_uri_target_aliasing_configured_db_is_rejected() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let configured = tmp.path().join("main.db");
        std::fs::write(&configured, b"").expect("create sentinel file");
        let uri = format!("file:{}?mode=rw", configured.display());
        let err = resolve_target_db(Some(&uri), Path::new("/tmp/some-repo"), Some(&configured))
            .expect_err("file: URI aliasing the configured db must be rejected");
        assert!(err.contains("shared production database"));
    }

    /// Same as above with a bare `file:` URI carrying no query parameters at
    /// all -- the URI-detection must not depend on a `?` being present.
    #[test]
    fn bare_sqlite_uri_target_aliasing_configured_db_is_rejected() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let configured = tmp.path().join("main.db");
        std::fs::write(&configured, b"").expect("create sentinel file");
        let uri = format!("file:{}", configured.display());
        let err = resolve_target_db(Some(&uri), Path::new("/tmp/some-repo"), Some(&configured))
            .expect_err("bare file: URI aliasing the configured db must be rejected");
        assert!(err.contains("shared production database"));
    }

    /// A distinct, non-forbidden `file:` URI target must still work -- the
    /// fence must not reject every URI-shaped input outright.
    #[test]
    fn sqlite_uri_target_to_dedicated_db_is_accepted() {
        let db = resolve_target_db(
            Some("file:/tmp/code-ingest-uri-map.db?mode=rwc"),
            Path::new("/tmp/some-repo"),
            None,
        )
        .expect("dedicated file: URI target accepted");
        assert_eq!(
            db,
            PathBuf::from("file:/tmp/code-ingest-uri-map.db?mode=rwc")
        );
    }

    /// An unparseable `file:` URI (empty path component) must fail closed
    /// rather than silently falling back to comparing the raw literal.
    #[test]
    fn unparseable_sqlite_uri_target_is_rejected() {
        let err = resolve_target_db(Some("file:?mode=rw"), Path::new("/tmp/some-repo"), None)
            .expect_err("unparseable file: URI must be refused, not passed through");
        assert!(err.contains("unparseable"), "unexpected error: {err}");
    }

    /// A hard link to the configured production database file
    /// has a DIFFERENT literal path from the original (no symlink involved,
    /// so `normalize`'s canonicalize-based comparison sees two distinct,
    /// already-canonical paths) but shares its `(device, inode)` identity —
    /// the fence must catch this via metadata, not path text.
    #[test]
    #[cfg(unix)]
    fn hard_link_to_configured_db_is_rejected() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let configured = tmp.path().join("main.db");
        std::fs::write(&configured, b"").expect("create sentinel file");
        let candidate = tmp.path().join("map.db");
        std::fs::hard_link(&configured, &candidate).expect("hard link");

        let err = resolve_target_db(
            Some(candidate.to_str().unwrap()),
            Path::new("/tmp/some-repo"),
            Some(&configured),
        )
        .expect_err("hard-linked alias of the configured db must be rejected");
        assert!(err.contains("shared production database"));
    }

    /// Serializes tests that mutate the process-wide `KHIVE_DB` env var —
    /// `std::env::set_var`/`remove_var` race across parallel `cargo test`
    /// threads otherwise.
    static KHIVE_DB_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn env_khive_db_is_fenced_when_config_db_path_is_unresolved() {
        let _guard = KHIVE_DB_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().expect("tempdir");
        let env_db = tmp.path().join("env-configured.db");
        std::fs::write(&env_db, b"").expect("create sentinel file");
        // SAFETY: serialized by KHIVE_DB_ENV_LOCK above.
        unsafe {
            std::env::set_var("KHIVE_DB", &env_db);
        }
        let result = resolve_target_db(
            Some(env_db.to_str().unwrap()),
            Path::new("/tmp/some-repo"),
            None, // config().db_path unresolved — the #1042 gap
        );
        unsafe {
            std::env::remove_var("KHIVE_DB");
        }
        let err = result.expect_err("KHIVE_DB must be fenced even when runtime_db_path is None");
        assert!(err.contains("shared production database"));
    }

    #[test]
    fn dedicated_path_still_accepted_with_no_khive_db_env_and_unresolved_config() {
        let _guard = KHIVE_DB_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: serialized by KHIVE_DB_ENV_LOCK above.
        unsafe {
            std::env::remove_var("KHIVE_DB");
        }
        let db = resolve_target_db(
            Some("/tmp/code-ingest-map-2.db"),
            Path::new("/tmp/some-repo"),
            None,
        )
        .expect("dedicated path accepted with no env override");
        assert_eq!(db, PathBuf::from("/tmp/code-ingest-map-2.db"));
    }

    /// RAII guard: clears `HOME` for the test body and restores the prior
    /// value on drop (including on panic/unwind) — mirrors the `HomeGuard`
    /// pattern in `khive-mcp/src/serve.rs`. Callers must hold
    /// `KHIVE_DB_ENV_LOCK` for the guard's whole lifetime; this type does not
    /// take the lock itself.
    struct HomeGuard {
        original: Option<std::ffi::OsString>,
    }

    impl HomeGuard {
        fn clear() -> Self {
            let original = std::env::var_os("HOME");
            // SAFETY: caller holds KHIVE_DB_ENV_LOCK for this guard's lifetime.
            unsafe {
                std::env::remove_var("HOME");
            }
            Self { original }
        }
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            // SAFETY: caller holds KHIVE_DB_ENV_LOCK for this guard's lifetime.
            unsafe {
                match &self.original {
                    Some(h) => std::env::set_var("HOME", h),
                    None => std::env::remove_var("HOME"),
                }
            }
        }
    }

    /// #1087 item 2 (TOCTOU) regression: the fence's own open-time check
    /// (`verify_opened_target`, run once right after open) cannot see a swap
    /// that happens AFTER it ran -- this is exactly why `code.ingest`'s
    /// handler re-verifies a second time immediately before the write-heavy
    /// ingest phase begins. This test proves that second re-verification
    /// mechanism actually catches the swap: the target path passes an
    /// initial check as a distinct file, is then replaced with a hard link
    /// to the forbidden configured database, and a second check on the same
    /// path must now reject it.
    #[test]
    #[cfg(unix)]
    fn reverification_catches_a_post_verify_swap_to_alias_the_configured_db() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let configured = tmp.path().join("main.db");
        std::fs::write(&configured, b"").expect("create sentinel file");
        let target = tmp.path().join("map.db");
        std::fs::write(&target, b"").expect("create distinct dedicated file");

        verify_opened_target(&target, Some(&configured))
            .expect("distinct dedicated file passes the first check");

        // Simulate an attacker swapping the target path, between the
        // open-time check above and the write phase, into a hard-linked
        // alias of the forbidden database.
        std::fs::remove_file(&target).expect("remove pre-swap target");
        std::fs::hard_link(&configured, &target).expect("swap in a hard-linked alias");

        let err = verify_opened_target(&target, Some(&configured))
            .expect_err("re-verification must catch the post-swap alias and fail closed");
        assert!(err.contains("shared production database"));
    }

    /// #1062 H2 regression: with `HOME` unset AND no `KHIVE_DB` override, the
    /// canonical resolver (`khive_runtime::config::resolve_db_anchor(None)`)
    /// still falls back to `./.khive/khive.db` — the fence's default
    /// forbidden path must be derived the SAME way, or this exact
    /// unresolved-config branch resolves the production db and lets it
    /// through (the prior HOME-only `default_production_db_path` returned
    /// `None` here, disarming the fence entirely).
    #[test]
    fn production_default_is_fenced_when_home_unset_and_khive_db_absent() {
        let _guard = KHIVE_DB_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _home_guard = HomeGuard::clear();
        // SAFETY: serialized by KHIVE_DB_ENV_LOCK above.
        unsafe {
            std::env::remove_var("KHIVE_DB");
        }
        let prod = khive_runtime::config::resolve_db_anchor(None)
            .expect("resolve_db_anchor(None) always resolves to Some(_)");
        let err = resolve_target_db(
            Some(prod.to_str().unwrap()),
            Path::new("/tmp/some-repo"),
            None, // config().db_path unresolved — the #1042/#1062 gap
        )
        .expect_err("must reject the canonical production db even with HOME unset");
        assert!(err.contains("shared production database"));
    }

    /// Same #1062 H2 gap, with `KHIVE_DB` present but empty rather than
    /// absent — the empty-string guard on the `KHIVE_DB` fallback (added for
    /// #1042) must not be mistaken for "no override, so allow it through";
    /// the canonical-default fence below it still has to fire.
    #[test]
    fn production_default_is_fenced_when_home_unset_and_khive_db_empty() {
        let _guard = KHIVE_DB_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _home_guard = HomeGuard::clear();
        // SAFETY: serialized by KHIVE_DB_ENV_LOCK above.
        unsafe {
            std::env::set_var("KHIVE_DB", "");
        }
        let prod = khive_runtime::config::resolve_db_anchor(None)
            .expect("resolve_db_anchor(None) always resolves to Some(_)");
        let result = resolve_target_db(
            Some(prod.to_str().unwrap()),
            Path::new("/tmp/some-repo"),
            None,
        );
        // SAFETY: serialized by KHIVE_DB_ENV_LOCK above.
        unsafe {
            std::env::remove_var("KHIVE_DB");
        }
        let err = result
            .expect_err("must reject the canonical production db with HOME unset + empty KHIVE_DB");
        assert!(err.contains("shared production database"));
    }

    /// #1087 item 3/8: `file:///C:/...` and `file://localhost/C:/...` both
    /// decode (per RFC 8089) to a POSIX-shaped `/C:/...` path, but SQLite's
    /// own URI parser strips the leading slash before a drive letter and
    /// opens `C:/...` -- the fence must compare against that same stripped
    /// form, or a `db=file:///C:/...` target aliasing the configured
    /// production database slips through uncaught.
    #[test]
    fn windows_drive_letter_file_uri_forms_resolve_to_the_same_path_sqlite_opens() {
        let expected = PathBuf::from("C:/Users/khive/.khive/khive.db");
        for raw in [
            "file:///C:/Users/khive/.khive/khive.db",
            "file://localhost/C:/Users/khive/.khive/khive.db",
        ] {
            let resolved =
                sqlite_uri_path(raw).unwrap_or_else(|| panic!("{raw:?} must parse to a path"));
            assert_eq!(
                resolved, expected,
                "{raw:?} must resolve to the drive-rooted path SQLite actually opens"
            );
        }
    }

    /// End-to-end fence-equality proof for the same bypass: a `db=` target
    /// spelled as a `file:///C:/...` URI aliasing the configured production
    /// database must be rejected exactly like the plain-path form is.
    #[test]
    fn windows_drive_letter_file_uri_aliasing_configured_db_is_rejected() {
        let configured = Path::new("C:/Users/khive/.khive/khive.db");
        for raw in [
            "file:///C:/Users/khive/.khive/khive.db",
            "file://localhost/C:/Users/khive/.khive/khive.db",
        ] {
            let err = resolve_target_db(Some(raw), Path::new("/tmp/some-repo"), Some(configured))
                .expect_err(
                    "windows drive-letter file: URI aliasing the configured db must be rejected",
                );
            assert!(
                err.contains("shared production database"),
                "unexpected error for {raw:?}: {err}"
            );
        }
    }

    /// A POSIX path must never be mistaken for the Windows drive-letter URI
    /// shape -- only exercised for `sqlite_uri_path`, which only ever sees
    /// URI-prefixed input, but pins the boundary of the drive-letter check
    /// itself.
    #[test]
    fn non_windows_uri_paths_are_left_unstripped() {
        assert_eq!(
            sqlite_uri_path("file:/tmp/code-ingest-map.db"),
            Some(PathBuf::from("/tmp/code-ingest-map.db"))
        );
    }
}
