use std::path::{Component, Path, PathBuf};

fn normalize(path: &Path) -> Result<PathBuf, String> {
    let mut pending = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| format!("resolving database path: {error}"))?
            .join(path)
    };
    // Inspect each component without following its final link: exists() hides
    // dangling links, including directory links before a missing suffix.
    'resolve: for _ in 0..=40 {
        let mut result = PathBuf::new();
        let mut components = pending.components();
        while let Some(component) = components.next() {
            match component {
                Component::Prefix(_) | Component::RootDir => result.push(component.as_os_str()),
                Component::CurDir => {}
                Component::ParentDir => {
                    result.pop();
                }
                Component::Normal(part) => {
                    result.push(part);
                    match std::fs::symlink_metadata(&result) {
                        Ok(metadata) if metadata.file_type().is_symlink() => {
                            let target = std::fs::read_link(&result).map_err(|error| {
                                format!("resolving database symlink {}: {error}", result.display())
                            })?;
                            let mut rewritten = if target.is_absolute() {
                                target
                            } else {
                                result
                                    .parent()
                                    .ok_or_else(|| {
                                        format!("cannot resolve database path {}", path.display())
                                    })?
                                    .join(target)
                            };
                            rewritten.push(components.as_path());
                            pending = rewritten;
                            continue 'resolve;
                        }
                        Ok(_) => {
                            result = result.canonicalize().map_err(|error| {
                                format!("resolving database path {}: {error}", path.display())
                            })?;
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => {
                            return Err(format!(
                                "resolving database path {}: {error}",
                                path.display()
                            ));
                        }
                    }
                }
            }
        }
        return Ok(result);
    }
    Err(format!(
        "cannot resolve database path {}: symlink chain exceeds 40 links",
        path.display()
    ))
}

fn same_file(a: &Path, b: &Path) -> Result<bool, String> {
    if normalize(a)? == normalize(b)? {
        return Ok(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let (Ok(a), Ok(b)) = (a.metadata(), b.metadata()) {
            return Ok(a.dev() == b.dev() && a.ino() == b.ino());
        }
    }
    Ok(false)
}

pub(crate) fn resolve_target_db(
    db: Option<&str>,
    source: &Path,
    runtime_db: Option<&Path>,
) -> Result<PathBuf, String> {
    if db.is_some_and(|value| value.trim().is_empty()) {
        return Err("web.ingest db must not be empty".to_string());
    }
    let candidate = db
        .map(PathBuf::from)
        .unwrap_or_else(|| source.join(".khive/web-map.db"));
    if candidate
        .as_os_str()
        .as_encoded_bytes()
        .get(..5)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"file:"))
    {
        return Err("web.ingest db must be a filesystem path, not a SQLite URI".to_string());
    }
    let mut forbidden = Vec::new();
    if let Some(default) = khive_runtime::config::resolve_db_anchor(None) {
        forbidden.push(default);
    }
    if let Some(runtime_db) = runtime_db {
        forbidden.push(runtime_db.to_path_buf());
    } else if let Some(env_db) = std::env::var_os("KHIVE_DB").filter(|value| !value.is_empty()) {
        forbidden.push(env_db.into());
    }
    for production in forbidden {
        if same_file(&candidate, &production)? {
            return Err(format!(
                "web.ingest refuses to target the shared production database ({}); use a dedicated map database",
                production.display()
            ));
        }
    }
    Ok(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_a_dedicated_web_map() {
        let source = tempfile::tempdir().unwrap();
        assert_eq!(
            resolve_target_db(None, source.path(), None).unwrap(),
            source.path().join(".khive/web-map.db")
        );
    }

    #[test]
    fn sqlite_uri_spelling_is_refused_by_the_ingest_fence_itself() {
        let root = tempfile::tempdir().unwrap();
        let production = root.path().join("production.db");
        for prefix in ["file:", "FILE:"] {
            let uri = format!("{prefix}{}?mode=rwc", production.display());
            let error = resolve_target_db(Some(&uri), root.path(), Some(&production)).unwrap_err();
            assert!(error.contains("SQLite URI"));
        }
        assert!(!production.exists());
    }

    #[test]
    fn configured_production_database_is_refused_before_creation() {
        let source = tempfile::tempdir().unwrap();
        let production = source.path().join("production.db");
        let error =
            resolve_target_db(production.to_str(), source.path(), Some(&production)).unwrap_err();
        assert!(error.contains("shared production database"));
        assert!(!production.exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_and_hardlink_aliases_of_production_are_refused() {
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let production = real.join("production.db");
        assert!(resolve_target_db(
            alias.join("production.db").to_str(),
            root.path(),
            Some(&production)
        )
        .is_err());
        std::fs::write(&production, b"sentinel").unwrap();
        let hardlink = root.path().join("hardlink.db");
        std::fs::hard_link(&production, &hardlink).unwrap();
        assert!(resolve_target_db(hardlink.to_str(), root.path(), Some(&production)).is_err());
        assert_eq!(std::fs::read(&production).unwrap(), b"sentinel");
    }

    #[cfg(unix)]
    #[test]
    fn dangling_final_symlinks_to_absent_production_are_refused() {
        let root = tempfile::tempdir().unwrap();
        let production = root.path().join("production.db");
        for (name, target) in [
            ("absolute-link", production.clone()),
            ("relative-link", PathBuf::from("production.db")),
        ] {
            let alias = root.path().join(name);
            std::os::unix::fs::symlink(target, &alias).unwrap();
            let error =
                resolve_target_db(alias.to_str(), root.path(), Some(&production)).unwrap_err();
            assert!(error.contains("shared production database"), "{error}");
        }
        assert!(!production.exists());
    }

    #[cfg(unix)]
    #[test]
    fn dangling_intermediate_symlinks_to_absent_production_are_refused() {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("not-created");
        let production = missing.join("nested/production.db");
        for (name, target) in [
            ("absolute-directory-link", missing.clone()),
            ("relative-directory-link", PathBuf::from("not-created")),
        ] {
            let alias = root.path().join(name);
            std::os::unix::fs::symlink(target, &alias).unwrap();
            let candidate = alias.join("nested/production.db");
            let error =
                resolve_target_db(candidate.to_str(), root.path(), Some(&production)).unwrap_err();
            assert!(error.contains("shared production database"), "{error}");
        }
        assert!(!missing.exists());
    }

    #[cfg(unix)]
    #[test]
    fn relative_symlink_targets_resolve_from_the_link_parent() {
        let root = tempfile::tempdir().unwrap();
        let links = root.path().join("links");
        std::fs::create_dir(&links).unwrap();
        let production = root.path().join("missing/production.db");
        std::os::unix::fs::symlink("../missing", links.join("directory")).unwrap();
        std::os::unix::fs::symlink("directory/production.db", links.join("entry")).unwrap();
        let error = resolve_target_db(links.join("entry").to_str(), root.path(), Some(&production))
            .unwrap_err();
        assert!(error.contains("shared production database"), "{error}");
        assert!(!production.exists());
    }

    #[cfg(unix)]
    #[test]
    fn dangling_alias_of_an_unrelated_map_remains_allowed() {
        let root = tempfile::tempdir().unwrap();
        let alias = root.path().join("map-alias.db");
        let production = root.path().join("production.db");
        std::os::unix::fs::symlink("map.db", &alias).unwrap();
        assert_eq!(
            resolve_target_db(alias.to_str(), root.path(), Some(&production)).unwrap(),
            alias
        );
        assert!(!root.path().join("map.db").exists());
    }

    #[cfg(unix)]
    #[test]
    fn final_and_intermediate_symlink_cycles_fail_closed() {
        let root = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("second", root.path().join("first")).unwrap();
        std::os::unix::fs::symlink("first", root.path().join("second")).unwrap();
        let production = root.path().join("production.db");
        for suffix in ["first", "first/missing/map.db"] {
            let candidate = root.path().join(suffix);
            let error =
                resolve_target_db(candidate.to_str(), root.path(), Some(&production)).unwrap_err();
            assert!(error.contains("symlink chain exceeds 40 links"), "{error}");
        }
        assert!(!production.exists());
    }
}
