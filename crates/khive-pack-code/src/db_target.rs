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
#[path = "db_target_tests.rs"]
mod tests;
