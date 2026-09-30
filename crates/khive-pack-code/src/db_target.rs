//! `code.ingest` target-database selection (ADR-085 Amendment 2 B7).
//!
//! `code.ingest` never writes to the shared production graph: it defaults to
//! a dedicated map database colocated with the ingested path, and rejects an
//! explicit `db` that resolves to any production store known to this process,
//! including declared backends, event stores and SQLite companions. This is a
//! path-level courtesy preflight; it cannot prove which handles SQLite later opens.

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

pub(crate) fn configured_production_bases(
    runtime_db_path: Option<&Path>,
    declared_backend_db_paths: &[PathBuf],
) -> Result<Vec<PathBuf>, String> {
    let mut bases = Vec::new();
    if let Some(prod) = default_production_db_path() {
        bases.push(prod);
    }
    match runtime_db_path {
        Some(runtime_db) => bases.push(runtime_db.to_path_buf()),
        None => {
            if let Ok(env_db) = std::env::var("KHIVE_DB") {
                if !env_db.is_empty() {
                    bases.push(PathBuf::from(env_db));
                }
            }
        }
    }
    bases.extend_from_slice(declared_backend_db_paths);
    let cwd = std::env::current_dir().map_err(|error| {
        format!("code.ingest cannot resolve configured production paths: {error}")
    })?;
    for base in &mut bases {
        if !base.is_absolute() {
            *base = cwd.join(&base);
        }
    }
    bases.sort();
    bases.dedup();
    Ok(bases)
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

fn same_path(a: &Path, b: &Path) -> bool {
    normalize(a) == normalize(b)
}

fn existing_metadata(path: &Path) -> Result<Option<std::fs::Metadata>, String> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!(
            "code.ingest cannot establish target database identity for {}: {error}",
            path.display()
        )),
    }
}

fn same_existing_file(a: &Path, b: &Path) -> Result<Option<bool>, String> {
    let Some(a_metadata) = existing_metadata(a)? else {
        return Ok(None);
    };
    let Some(b_metadata) = existing_metadata(b)? else {
        return Ok(None);
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(Some(
            a_metadata.dev() == b_metadata.dev() && a_metadata.ino() == b_metadata.ino(),
        ))
    }
    #[cfg(windows)]
    {
        let _ = (a_metadata, b_metadata);
        // Stable std metadata does not expose a Windows file id. The helper
        // compares volume and file-index through read-only handles.
        same_file::is_same_file(a, b).map(Some).map_err(|error| {
            format!(
                "code.ingest cannot establish target database identity for {}: {error}",
                b.display()
            )
        })
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (a_metadata, b_metadata);
        Err("code.ingest cannot establish target database identity on this platform".to_string())
    }
}

fn matches_protected_file(candidate: &Path, protected: &Path) -> Result<bool, String> {
    Ok(
        same_existing_file(candidate, protected)?
            .unwrap_or_else(|| same_path(candidate, protected)),
    )
}

/// Include each final-component symlink destination, even when that destination
/// does not exist yet. `metadata` follows a dangling link and reports only
/// NotFound, so the ordinary missing-path comparison would otherwise compare
/// the link's name rather than the file SQLite will open through it.
fn target_spellings(candidate: &Path) -> Result<Vec<PathBuf>, String> {
    let mut spellings = vec![candidate.to_path_buf()];
    let mut current = candidate.to_path_buf();
    for _ in 0..40 {
        let metadata = match std::fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(spellings),
            Err(error) => {
                return Err(format!(
                    "code.ingest cannot establish target database identity for {}: {error}",
                    current.display()
                ));
            }
        };
        if !metadata.file_type().is_symlink() {
            return Ok(spellings);
        }
        let destination = std::fs::read_link(&current).map_err(|error| {
            format!(
                "code.ingest cannot establish target database identity for {}: {error}",
                current.display()
            )
        })?;
        current = if destination.is_absolute() {
            destination
        } else {
            current
                .parent()
                .unwrap_or_else(|| Path::new(""))
                .join(destination)
        };
        if spellings.contains(&current) {
            return Err(format!(
                "code.ingest cannot establish target database identity for {}: symlink loop",
                candidate.display()
            ));
        }
        spellings.push(current.clone());
    }
    Err(format!(
        "code.ingest cannot establish target database identity for {}: too many symlinks",
        candidate.display()
    ))
}

fn append_sqlite_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn protected_store_members(base: &Path) -> Result<Vec<PathBuf>, String> {
    let canonical = match base.canonicalize() {
        Ok(path) => Some(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(format!(
                "code.ingest cannot establish target database identity for {}: {error}",
                base.display()
            ));
        }
    };
    let mut bases = vec![base.to_path_buf()];
    if let Some(physical) = canonical {
        if physical.as_path() != base {
            bases.push(physical);
        }
    } else {
        // A missing final symlink target still names the database SQLite will
        // create; protect its companions beside that destination as well.
        bases.extend(target_spellings(base)?.into_iter().skip(1));
    }
    let mut members = Vec::new();
    for spelling in &bases {
        let events = khive_runtime::events_split::events_db_path_beside(spelling);
        for db in [spelling, events.as_path()] {
            members.extend([
                db.to_path_buf(),
                append_sqlite_suffix(db, "-journal"),
                append_sqlite_suffix(db, "-wal"),
                append_sqlite_suffix(db, "-shm"),
            ]);
        }
    }
    Ok(members)
}

/// Reject SQLite URI spellings and relative explicit targets before filesystem
/// access (ADR-085 E7). This is syntax admission, not an open-time identity fence.
pub(crate) fn validate_explicit_db_path(db: &str) -> Result<(), String> {
    if db.starts_with("file:") || db.contains('?') || !Path::new(db).is_absolute() {
        return Err(
            "code.ingest refuses this database target: explicit db must be an absolute, plain \
             filesystem path (no file: URI or ? query syntax)"
                .to_string(),
        );
    }
    Ok(())
}

/// Resolve the `db` verb argument into a concrete target database path,
/// defaulting to `<path>/.khive/code-map.db` when absent, and rejecting a
/// target that resolves to a production store known to this process: the
/// default anchor, the calling runtime's configured database (or `KHIVE_DB`
/// if unresolved), and every declared backend. Each store's event database
/// and SQLite companions are protected as well.
/// An explicit target must already be a regular file. The final component of
/// either target may not be a symlink. This prevents typo-driven creation and
/// ordinary link traversal before runtime construction; it does not pin
/// identity or protect against a swap between this check and SQLite open.
pub(crate) fn resolve_target_db(
    db_param: Option<&str>,
    ingest_path: &Path,
    runtime_db_path: Option<&Path>,
    declared_backend_db_paths: &[PathBuf],
) -> Result<PathBuf, String> {
    let candidate = match db_param {
        Some(p) => {
            validate_explicit_db_path(p)?;
            PathBuf::from(p)
        }
        None => ingest_path
            .canonicalize()
            .map_err(|error| {
                format!(
                    "code.ingest cannot pin omitted-db workspace parent {}: {error}",
                    ingest_path.display()
                )
            })?
            .join(".khive")
            .join("code-map.db"),
    };

    let forbidden = configured_production_bases(runtime_db_path, declared_backend_db_paths)?;
    let candidate_spellings = target_spellings(&candidate)?;
    for base in &forbidden {
        for forbidden_path in protected_store_members(base)? {
            for spelling in &candidate_spellings {
                if matches_protected_file(spelling, &forbidden_path)? {
                    return Err(format!(
                        "code.ingest refuses to target the shared production database ({}); \
                         select a different dedicated map database",
                        forbidden_path.display()
                    ));
                }
            }
        }
    }
    // Do not follow the final component here. The earlier production identity
    // census retains its more specific protected-store refusal for links to a
    // known production member; this check also refuses links to dedicated maps.
    let metadata = match std::fs::symlink_metadata(&candidate) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && db_param.is_none() => None,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!(
                "code.ingest explicit db {candidate:?} must be an existing regular file: {error}; \
                 omit db to create the workspace-local default"
            ));
        }
        Err(error) => {
            return Err(format!(
                "code.ingest cannot establish target database identity for {}: {error}",
                candidate.display()
            ));
        }
    };
    if metadata
        .as_ref()
        .is_some_and(|metadata| metadata.file_type().is_symlink())
    {
        return Err(format!(
            "code.ingest refuses final-component symlink target {}; \
             select a regular dedicated map database",
            candidate.display()
        ));
    }
    if db_param.is_some() && !metadata.as_ref().is_some_and(std::fs::Metadata::is_file) {
        return Err(format!(
            "code.ingest explicit db {candidate:?} must be an existing regular file; \
             omit db to create the workspace-local default"
        ));
    }
    Ok(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    // MUST-FAIL: bypassing syntax admission reaches the existing-file probe and
    // returns its error instead. Neither URI nor relative spellings are paths
    // this resolver may probe, even when the named file is missing.
    #[test]
    fn explicit_target_syntax_refuses_before_target_probe() {
        let tmp = tempfile::tempdir().expect("isolated target fixture");
        let missing = tmp.path().join("missing.db");
        for db in [
            format!("file:{}", missing.display()),
            format!("file:{}?mode=rw", missing.display()),
            format!("{}?mode=rw", missing.display()),
            "relative-map.db".to_string(),
            "".to_string(),
        ] {
            let error = resolve_target_db(Some(&db), tmp.path(), None, &[])
                .expect_err("non-plain or relative explicit target must refuse");
            assert!(error.contains("absolute, plain filesystem path"), "{error}");
        }
        assert!(!missing.exists());
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
    }

    #[test]
    fn default_target_is_workspace_local() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().canonicalize().unwrap().join("some-repo");
        std::fs::create_dir(&path).expect("create ingest workspace");
        let db = resolve_target_db(None, &path, None, &[]).expect("default resolves");
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
        let err = resolve_target_db(Some(&prod), Path::new("/tmp/some-repo"), None, &[])
            .expect_err("must reject the shared production database");
        assert!(err.contains("shared production database"));
    }

    #[test]
    fn explicit_dedicated_path_is_accepted() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let target = tmp.path().join("code-map.db");
        std::fs::write(&target, b"").expect("pre-create dedicated map");
        let db = resolve_target_db(
            Some(target.to_str().unwrap()),
            Path::new("/tmp/some-repo"),
            None,
            &[],
        )
        .expect("dedicated path accepted");
        assert_eq!(db, target);
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn final_component_symlinks_refuse_while_an_independent_copy_is_admitted() {
        fn link_file(target: &Path, link: &Path) {
            #[cfg(unix)]
            std::os::unix::fs::symlink(target, link).expect("create file symlink");
            #[cfg(windows)]
            std::os::windows::fs::symlink_file(target, link).expect(
                "Windows CI needs Developer Mode or symlink privilege; do not skip this witness",
            );
        }

        let tmp = tempfile::tempdir().expect("tempdir");
        let protected = tmp.path().join("protected.db");
        let dedicated = tmp.path().join("dedicated.db");
        std::fs::write(&protected, b"protected bytes").unwrap();
        std::fs::write(&dedicated, b"dedicated bytes").unwrap();

        let explicit_link = tmp.path().join("explicit-link.db");
        link_file(&dedicated, &explicit_link);
        let error = resolve_target_db(
            Some(explicit_link.to_str().unwrap()),
            tmp.path(),
            Some(&protected),
            &[],
        )
        .expect_err("an explicit final-component symlink must refuse");
        assert!(error.contains("final-component symlink"), "{error}");

        let ingest = tmp.path().join("source");
        let default_dir = ingest.join(".khive");
        std::fs::create_dir_all(&default_dir).unwrap();
        let default_link = default_dir.join("code-map.db");
        link_file(&dedicated, &default_link);
        let error = resolve_target_db(None, &ingest, Some(&protected), &[])
            .expect_err("an existing default final-component symlink must refuse");
        assert!(error.contains("final-component symlink"), "{error}");

        let protected_link = tmp.path().join("protected-link.db");
        link_file(&protected, &protected_link);
        assert!(
            resolve_target_db(
                Some(protected_link.to_str().unwrap()),
                tmp.path(),
                Some(&protected),
                &[],
            )
            .is_err(),
            "a symlink to production must also refuse"
        );

        let independent_copy = tmp.path().join("independent-copy.db");
        std::fs::copy(&protected, &independent_copy).unwrap();
        assert_eq!(
            resolve_target_db(
                Some(independent_copy.to_str().unwrap()),
                tmp.path(),
                Some(&protected),
                &[],
            ),
            Ok(independent_copy)
        );
        assert_eq!(std::fs::read(&protected).unwrap(), b"protected bytes");
        assert_eq!(std::fs::read(&dedicated).unwrap(), b"dedicated bytes");
        assert!(std::fs::symlink_metadata(&explicit_link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(std::fs::symlink_metadata(&default_link)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn declared_backend_and_its_event_companions_are_protected() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let backend = tmp.path().join("secondary.db");
        let events = khive_runtime::events_split::events_db_path_beside(&backend);
        for target in [
            backend.clone(),
            append_sqlite_suffix(&backend, "-journal"),
            append_sqlite_suffix(&backend, "-wal"),
            append_sqlite_suffix(&backend, "-shm"),
            events.clone(),
            append_sqlite_suffix(&events, "-journal"),
            append_sqlite_suffix(&events, "-wal"),
            append_sqlite_suffix(&events, "-shm"),
        ] {
            std::fs::write(&target, b"unchanged").expect("sentinel file");
            let error = resolve_target_db(
                Some(target.to_str().expect("utf8 temp path")),
                tmp.path(),
                None,
                std::slice::from_ref(&backend),
            )
            .expect_err("known production member must refuse");
            assert!(error.contains(&target.display().to_string()), "{error}");
            assert_eq!(std::fs::read(&target).unwrap(), b"unchanged");
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_backend_physical_wal_companion_is_protected() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let declared_dir = tmp.path().join("declared");
        let physical_dir = tmp.path().join("physical");
        std::fs::create_dir_all(&declared_dir).unwrap();
        std::fs::create_dir_all(&physical_dir).unwrap();
        let physical = physical_dir.join("real.db");
        let declared = declared_dir.join("khive.db");
        let physical_wal = append_sqlite_suffix(&physical, "-wal");
        std::fs::write(&physical, b"backend").unwrap();
        std::fs::write(&physical_wal, b"wal sentinel").unwrap();
        std::os::unix::fs::symlink(&physical, &declared).unwrap();

        let error = resolve_target_db(
            Some(physical_wal.to_str().unwrap()),
            tmp.path(),
            None,
            &[declared],
        )
        .expect_err("the physical SQLite WAL companion must be protected");
        assert!(
            error.contains(&physical_wal.display().to_string()),
            "{error}"
        );
        assert_eq!(std::fs::read(&physical_wal).unwrap(), b"wal sentinel");
    }

    #[cfg(unix)]
    #[test]
    fn dangling_backend_physical_wal_companion_is_protected() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let physical = tmp.path().join("physical.db");
        let declared = tmp.path().join("declared.db");
        let physical_wal = append_sqlite_suffix(&physical, "-wal");
        std::fs::write(&physical_wal, b"wal sentinel").unwrap();
        std::os::unix::fs::symlink(&physical, &declared).unwrap();
        assert!(!physical.exists());

        let error = resolve_target_db(
            Some(physical_wal.to_str().unwrap()),
            tmp.path(),
            None,
            &[declared],
        )
        .expect_err("a dangling backend must protect the physical WAL companion");
        assert!(
            error.contains(&physical_wal.display().to_string()),
            "{error}"
        );
        assert_eq!(std::fs::read(&physical_wal).unwrap(), b"wal sentinel");
        assert!(!physical.exists());
    }

    #[cfg(unix)]
    #[test]
    fn dangling_production_backend_and_default_map_share_destination_is_refused() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let physical = tmp.path().join("physical.db");
        let declared = tmp.path().join("declared.db");
        std::os::unix::fs::symlink(&physical, &declared).unwrap();

        let ingest = tmp.path().join("source");
        let default_dir = ingest.join(".khive");
        std::fs::create_dir_all(&default_dir).unwrap();
        let default_map = default_dir.join("code-map.db");
        std::os::unix::fs::symlink(&physical, &default_map).unwrap();
        assert!(!physical.exists());

        let error = resolve_target_db(None, &ingest, Some(&declared), &[])
            .expect_err("a default map must not create a dangling production destination");
        assert!(error.contains(&physical.display().to_string()), "{error}");
        assert!(!physical.exists());
        assert_eq!(std::fs::read_link(&default_map).unwrap(), physical);
    }

    #[cfg(unix)]
    #[test]
    fn dangling_default_symlink_to_protected_events_store_is_refused() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let backend = tmp.path().join("backend.db");
        let events = khive_runtime::events_split::events_db_path_beside(&backend);
        let ingest = tmp.path().join("source");
        let default_dir = ingest.join(".khive");
        std::fs::create_dir_all(&default_dir).unwrap();
        let default_map = default_dir.join("code-map.db");
        let relative_events = Path::new("../..").join(events.file_name().unwrap());
        std::os::unix::fs::symlink(&relative_events, &default_map).unwrap();
        assert!(!events.exists());

        let error = resolve_target_db(None, &ingest, Some(&backend), &[])
            .expect_err("a dangling default link must not create a protected event store");
        assert!(error.contains(&events.display().to_string()), "{error}");
        assert!(!events.exists());
        assert_eq!(std::fs::read_link(&default_map).unwrap(), relative_events);
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn hardlink_alias_is_protected_but_byte_copy_is_a_distinct_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let backend = tmp.path().join("secondary.db");
        let alias = tmp.path().join("alias.db");
        let copy = tmp.path().join("copy.db");
        std::fs::write(&backend, b"same bytes").unwrap();
        std::fs::hard_link(&backend, &alias).unwrap();
        std::fs::copy(&backend, &copy).unwrap();
        let known = [backend];
        let error = resolve_target_db(Some(alias.to_str().unwrap()), tmp.path(), None, &known)
            .expect_err("hard link has the same file identity");
        assert!(error.contains(&known[0].display().to_string()), "{error}");
        assert_eq!(
            resolve_target_db(Some(copy.to_str().unwrap()), tmp.path(), None, &known),
            Ok(copy)
        );
    }

    #[cfg(unix)]
    #[test]
    fn identity_probe_error_refuses_instead_of_falling_back_to_pathnames() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let target = tmp.path().join("dedicated.db");
        let backend = tmp.path().join("backend.db");
        let loop_target = tmp.path().join("loop.db");
        std::fs::write(&target, b"").unwrap();
        std::os::unix::fs::symlink(&loop_target, &backend).unwrap();
        std::os::unix::fs::symlink(&backend, &loop_target).unwrap();

        let error = resolve_target_db(Some(target.to_str().unwrap()), tmp.path(), None, &[backend])
            .expect_err("an uninspectable production member cannot be ignored");
        assert!(
            error.contains("cannot establish target database identity"),
            "{error}"
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"");
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
            &[],
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
            &[],
        )
        .expect_err("symlinked-parent alias of the configured db must be rejected");
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
            &[],
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
        let tmp = tempfile::tempdir().expect("tempdir");
        let target = tmp.path().join("code-map.db");
        std::fs::write(&target, b"").expect("pre-create dedicated map");
        let db = resolve_target_db(
            Some(target.to_str().unwrap()),
            Path::new("/tmp/some-repo"),
            None,
            &[],
        )
        .expect("dedicated path accepted with no env override");
        assert_eq!(db, target);
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
        // Explicit targets now require absolute spelling; retain this test's
        // separate assertion that the HOME-less canonical default is fenced.
        let prod = std::env::current_dir().unwrap().join(prod);
        let err = resolve_target_db(
            Some(prod.to_str().unwrap()),
            Path::new("/tmp/some-repo"),
            None, // config().db_path unresolved — the #1042/#1062 gap
            &[],
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
        let prod = std::env::current_dir().unwrap().join(prod);
        let result = resolve_target_db(
            Some(prod.to_str().unwrap()),
            Path::new("/tmp/some-repo"),
            None,
            &[],
        );
        // SAFETY: serialized by KHIVE_DB_ENV_LOCK above.
        unsafe {
            std::env::remove_var("KHIVE_DB");
        }
        let err = result
            .expect_err("must reject the canonical production db with HOME unset + empty KHIVE_DB");
        assert!(err.contains("shared production database"));
    }
}
