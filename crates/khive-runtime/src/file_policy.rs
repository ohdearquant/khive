//! Confined server file destinations shared by result sinks and blob verbs.

use std::path::{Path, PathBuf};

/// Environment override for the allowed `save_to` export root.
pub const EXPORT_ROOT_ENV: &str = "KHIVE_SAVE_TO_ROOT";

/// Resolve (and create) the allowed export root for `save_to` destinations.
/// Defaults to `~/.khive/exports`; overridable via `KHIVE_SAVE_TO_ROOT`.
pub fn export_root() -> anyhow::Result<PathBuf> {
    let root = match std::env::var(EXPORT_ROOT_ENV) {
        Ok(v) if !v.trim().is_empty() => PathBuf::from(v),
        _ => {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            PathBuf::from(home).join(".khive").join("exports")
        }
    };
    std::fs::create_dir_all(&root)
        .map_err(|e| anyhow::anyhow!("create export root {}: {e}", root.display()))?;
    root.canonicalize()
        .map_err(|e| anyhow::anyhow!("canonicalize export root {}: {e}", root.display()))
}

/// Validate a client-supplied `save_to` path against the allowed export `root`
/// and return the canonicalized destination. Rejects `..` traversal, a
/// resolved parent outside `root`, and an existing symlink at the
/// destination. See `crates/khive-mcp/docs/save-sink.md`.
pub fn validate_destination(root: &Path, requested: &Path) -> anyhow::Result<PathBuf> {
    if requested.as_os_str().is_empty() {
        anyhow::bail!("save_to path must not be empty");
    }
    if requested
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        anyhow::bail!(
            "save_to path must not contain '..' traversal components: {}",
            requested.display()
        );
    }

    let joined = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        root.join(requested)
    };

    let parent = joined.parent().filter(|p| !p.as_os_str().is_empty());
    let parent = match parent {
        Some(p) => p,
        None => anyhow::bail!("save_to path has no parent directory: {}", joined.display()),
    };

    // Containment must be proven BEFORE any directory creation: walk up to the
    // deepest existing ancestor and canonicalize that. `..` components were
    // already rejected above, so the not-yet-existing suffix can only descend
    // beneath the ancestor — if the ancestor is inside the root, the parent is.
    let mut existing = parent;
    while !existing.exists() {
        existing = match existing.parent().filter(|p| !p.as_os_str().is_empty()) {
            Some(p) => p,
            None => anyhow::bail!(
                "save_to path has no existing ancestor: {}",
                joined.display()
            ),
        };
    }
    let canonical_existing = existing.canonicalize().map_err(|e| {
        anyhow::anyhow!("canonicalize save_to ancestor {}: {e}", existing.display())
    })?;
    if !canonical_existing.starts_with(root) {
        anyhow::bail!(
            "save_to path escapes the allowed export root ({}): {}",
            root.display(),
            joined.display()
        );
    }

    std::fs::create_dir_all(parent)
        .map_err(|e| anyhow::anyhow!("create save_to parent dir {}: {e}", parent.display()))?;

    let canonical_parent = parent
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("canonicalize save_to parent {}: {e}", parent.display()))?;

    if !canonical_parent.starts_with(root) {
        anyhow::bail!(
            "save_to path escapes the allowed export root ({}): {}",
            root.display(),
            joined.display()
        );
    }

    let file_name = joined
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("save_to path has no file name: {}", joined.display()))?;
    let dest = canonical_parent.join(file_name);

    if let Ok(meta) = std::fs::symlink_metadata(&dest) {
        if meta.file_type().is_symlink() {
            anyhow::bail!(
                "save_to destination must not be a symlink: {}",
                dest.display()
            );
        }
    }

    Ok(dest)
}

pub fn resolve_destination(path: &Path, restrict_to_export_root: bool) -> anyhow::Result<PathBuf> {
    if path.as_os_str().is_empty() {
        anyhow::bail!("save_to path must not be empty");
    }

    let destination = if restrict_to_export_root {
        let root = export_root()?;
        validate_destination(&root, path)
    } else {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| anyhow::anyhow!("create parent dir {}: {e}", parent.display()))?;
            }
        }
        Ok(path.to_path_buf())
    }?;

    match std::fs::symlink_metadata(&destination) {
        Ok(metadata) if !metadata.file_type().is_file() => anyhow::bail!(
            "save_to destination must be absent or an existing regular file: {}",
            destination.display()
        ),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => anyhow::bail!(
            "inspect save_to destination {}: {error}",
            destination.display()
        ),
    }

    Ok(destination)
}

/// Environment override for the confined server import directory.
pub const IMPORT_ROOT_ENV: &str = "KHIVE_IMPORT_FROM_ROOT";

/// Resolve (and create) the import root, defaulting to `~/.khive/imports`.
pub fn import_root() -> anyhow::Result<PathBuf> {
    let root = match std::env::var(IMPORT_ROOT_ENV) {
        Ok(value) if !value.trim().is_empty() => PathBuf::from(value),
        _ => {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            PathBuf::from(home).join(".khive").join("imports")
        }
    };
    std::fs::create_dir_all(&root)
        .map_err(|error| anyhow::anyhow!("create import root {}: {error}", root.display()))?;
    root.canonicalize()
        .map_err(|error| anyhow::anyhow!("canonicalize import root {}: {error}", root.display()))
}

/// Both file verbs require disjoint canonical roots at the time of the call.
pub fn confined_file_roots() -> anyhow::Result<(PathBuf, PathBuf)> {
    let imports = import_root()?;
    let exports = export_root()?;
    if imports.starts_with(&exports) || exports.starts_with(&imports) {
        anyhow::bail!("import and export roots must not be equal or nested");
    }
    Ok((imports, exports))
}

/// Validate an existing import file without accepting symlinks within the root.
pub fn validate_import(root: &Path, requested: &Path) -> anyhow::Result<PathBuf> {
    if requested.as_os_str().is_empty() {
        anyhow::bail!("import path must not be empty");
    }
    if requested
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        anyhow::bail!("import path must not contain '..' traversal components");
    }
    let joined = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        root.join(requested)
    };
    if !joined.starts_with(root) {
        anyhow::bail!(
            "import path escapes the allowed import root ({})",
            root.display()
        );
    }
    let mut current = root.to_path_buf();
    for component in joined.strip_prefix(root)?.components() {
        current.push(component);
        let metadata = std::fs::symlink_metadata(&current).map_err(|error| {
            anyhow::anyhow!("inspect import path {}: {error}", current.display())
        })?;
        if metadata.file_type().is_symlink() {
            anyhow::bail!(
                "import path must not contain a symlink: {}",
                current.display()
            );
        }
    }
    let canonical = joined.canonicalize().map_err(|error| {
        anyhow::anyhow!("canonicalize import file {}: {error}", joined.display())
    })?;
    if !canonical.starts_with(root) {
        anyhow::bail!(
            "import path escapes the allowed import root ({})",
            root.display()
        );
    }
    if !std::fs::metadata(&canonical)?.is_file() {
        anyhow::bail!("import source must be a regular file: {}", joined.display());
    }
    Ok(canonical)
}

/// Open the validated source and recheck the opened object's type.
pub fn open_import(root: &Path, requested: &Path) -> anyhow::Result<std::fs::File> {
    let path = validate_import(root, requested)?;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options
        .open(&path)
        .map_err(|error| anyhow::anyhow!("open import source {}: {error}", path.display()))?;
    if !file.metadata()?.is_file() {
        anyhow::bail!("import source must be a regular file: {}", path.display());
    }
    Ok(file)
}
