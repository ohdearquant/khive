//! Canonical path resolution that permits missing components.

use std::path::Path;

/// Canonicalize a path while permitting missing components.
///
/// Existing final leaves may be files or directories. Missing suffixes are
/// normalized through existing symlinks, including dangling links. Parent
/// components cancel prior components before subsequent components are inspected.
///
/// This observes current filesystem topology only. It does not lock later path
/// replacements or guarantee containment or freedom from time-of-check/time-of-use
/// races.
pub fn canonicalize_allow_missing(path: &Path) -> std::io::Result<std::path::PathBuf> {
    use std::collections::VecDeque;
    use std::path::Component;

    let original_not_found = match std::fs::canonicalize(path) {
        Ok(resolved) => return Ok(resolved),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => error,
        Err(error) => return Err(error),
    };
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    // Windows drive-relative paths can replace rather than extend current_dir.
    if !absolute.is_absolute() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "export destination must resolve to a fully rooted path",
        ));
    }
    let mut pending: VecDeque<_> = absolute
        .components()
        .map(|part| part.as_os_str().to_os_string())
        .collect();
    let mut resolved = std::path::PathBuf::new();
    let mut followed_links = 0usize;
    while let Some(part) = pending.pop_front() {
        match Path::new(&part).components().next() {
            Some(Component::Prefix(_)) | Some(Component::RootDir) => resolved.push(&part),
            Some(Component::CurDir) => {}
            Some(Component::ParentDir) => {
                resolved.pop();
            }
            Some(Component::Normal(_)) => {
                resolved.push(&part);
                // Reinspect every component, including components reached after
                // ".." cancels an absent directory. Never append that tail blindly.
                match std::fs::symlink_metadata(&resolved) {
                    Ok(metadata) if metadata.file_type().is_symlink() => {
                        followed_links += 1;
                        if followed_links > 40 {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::InvalidInput,
                                "too many symlinks while resolving export destination",
                            ));
                        }
                        let target = std::fs::read_link(&resolved)?;
                        resolved.pop();
                        if target.is_absolute() {
                            resolved.clear();
                        }
                        for target_part in target.components().rev() {
                            pending.push_front(target_part.as_os_str().to_os_string());
                        }
                    }
                    Ok(metadata) => {
                        if let Some(next) = pending.front().filter(|_| !metadata.is_dir()) {
                            // Keep a native traversal error when the next step fails.
                            // If a nondirectory/.. reprobe succeeds, retain the original
                            // whole-input NotFound instead of synthesizing an error.
                            let next = resolved.join(next);
                            return match std::fs::canonicalize(next) {
                                Err(error) => Err(error),
                                Ok(_) => Err(original_not_found),
                            };
                        }
                        resolved = std::fs::canonicalize(&resolved)?;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
            }
            None => {}
        }
    }
    Ok(resolved)
}

#[cfg(test)]
#[path = "canonical_path_tests.rs"]
mod tests;
