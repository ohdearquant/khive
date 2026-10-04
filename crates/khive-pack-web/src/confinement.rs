//! ADR-191 A1.1: admit and read disk entries through the same opened descriptors.

use std::fs::File;
use std::path::{Path, PathBuf};

use khive_runtime::{
    bounded_read::read_to_end_bounded, engine_config::WebSectionConfig, RuntimeError,
};

use crate::egress::Refusal;

#[derive(Debug)]
pub(crate) struct OpenedFile {
    pub relative: PathBuf,
    file: File,
}

impl OpenedFile {
    pub fn read(mut self, max_bytes: u64) -> Result<Vec<u8>, RuntimeError> {
        read_to_end_bounded(&mut self.file, max_bytes)
            .map_err(|error| refusal("ingest_read_failed", &self.relative, error))?
            .ok_or_else(|| {
                refusal(
                    "ingest_file_too_large",
                    &self.relative,
                    format!("file exceeds the {max_bytes}-byte disk ingest ceiling"),
                )
            })
    }
}

fn refusal(code: &'static str, path: &Path, error: impl std::fmt::Display) -> RuntimeError {
    Refusal::new(code, format!("web.ingest: {}: {error}", path.display())).into()
}

/// Admit the complete tree, returning at most `limit` regular-file descriptors.
///
/// The limit bounds returned file descriptors and later body reads, not discovery.
/// Every entry is still inspected so an inadmissible entry anywhere in the source
/// causes refusal, including at limit zero. Each visited directory's names are
/// collected and sorted; retained name storage follows the active DFS stack.
/// Operators needing a discovery bound must choose a smaller source directory.
pub(crate) fn open_files(
    cfg: &WebSectionConfig,
    source: &Path,
    limit: u32,
    before_open: &mut dyn FnMut(&Path),
) -> Result<Vec<OpenedFile>, RuntimeError> {
    if cfg.read_roots.is_empty() {
        return Err(Refusal::new(
            "ingest_disk_no_read_roots",
            "web.ingest: disk ingest is refused because [web] read_roots is unset; configure at least one root to allow it",
        ).into());
    }
    platform::open_files(cfg, source, limit as usize, before_open)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod platform {
    use super::*;

    pub(super) fn open_files(
        _cfg: &WebSectionConfig,
        source: &Path,
        _limit: usize,
        _before_open: &mut dyn FnMut(&Path),
    ) -> Result<Vec<OpenedFile>, RuntimeError> {
        Err(refusal(
            "ingest_confinement_unsupported",
            source,
            "descriptor-based disk ingest confinement is unavailable on this platform",
        ))
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod platform {
    use super::*;
    use std::ffi::{OsStr, OsString};
    use std::io;
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::Component;

    use khive_fs::fd_relative::{list_names, open_at, stat_at, stat_fd};

    enum AdmissionError {
        DescriptorExhausted { path: PathBuf, error: io::Error },
        Other(RuntimeError),
    }

    impl From<AdmissionError> for RuntimeError {
        fn from(error: AdmissionError) -> Self {
            match error {
                AdmissionError::DescriptorExhausted { path, error } => {
                    super::refusal("ingest_descriptor_exhausted", &path, error)
                }
                AdmissionError::Other(error) => error,
            }
        }
    }

    fn refusal(code: &'static str, path: &Path, error: impl std::fmt::Display) -> AdmissionError {
        AdmissionError::Other(super::refusal(code, path, error))
    }

    fn io_refusal(code: &'static str, path: &Path, error: io::Error) -> AdmissionError {
        if matches!(
            error.raw_os_error(),
            Some(libc::EMFILE) | Some(libc::ENFILE)
        ) {
            AdmissionError::DescriptorExhausted {
                path: path.to_path_buf(),
                error,
            }
        } else {
            refusal(code, path, error)
        }
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    struct Identity {
        dev: libc::dev_t,
        ino: libc::ino_t,
    }

    impl From<&libc::stat> for Identity {
        fn from(stat: &libc::stat) -> Self {
            Self {
                dev: stat.st_dev,
                ino: stat.st_ino,
            }
        }
    }

    struct Directory {
        file: File,
        path: PathBuf,
        ancestry: Vec<(PathBuf, Identity)>,
    }

    fn check_type(stat: &libc::stat, path: &Path) -> Result<bool, AdmissionError> {
        match stat.st_mode & libc::S_IFMT {
            libc::S_IFDIR => Ok(true),
            libc::S_IFREG => Ok(false),
            libc::S_IFLNK => Err(refusal(
                "ingest_symlink_refused",
                path,
                "symbolic link refused",
            )),
            _ => Err(refusal(
                "ingest_file_type_refused",
                path,
                "only directories and regular files are permitted",
            )),
        }
    }

    fn open_checked(
        parent: &File,
        name: &OsStr,
        path: &Path,
        checked: &libc::stat,
        before_open: &mut dyn FnMut(&Path),
    ) -> Result<File, AdmissionError> {
        let directory = check_type(checked, path)?;
        before_open(path);
        // open_at sets O_NONBLOCK, which prevents a raced-in FIFO from blocking before
        // fstat rejects it.
        let opened = open_at(parent, name, directory)
            .map_err(|error| io_refusal("ingest_path_changed", path, error))?;
        let actual = stat_fd(&opened)
            .map_err(|error| io_refusal("ingest_path_unresolvable", path, error))?;
        if Identity::from(checked) != Identity::from(&actual)
            || checked.st_mode & libc::S_IFMT != actual.st_mode & libc::S_IFMT
        {
            return Err(refusal(
                "ingest_path_changed",
                path,
                "opened descriptor differs from the checked device/inode or file type",
            ));
        }
        Ok(opened)
    }

    fn open_directory(
        path: &Path,
        before_open: &mut dyn FnMut(&Path),
    ) -> Result<Directory, AdmissionError> {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map_err(|error| io_refusal("ingest_path_unresolvable", path, error))?
                .join(path)
        };
        // The filesystem root has no caller-controlled ancestor to follow.
        let root = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open("/")
            .map_err(|error| io_refusal("ingest_path_unresolvable", path, error))?;
        let identity = Identity::from(
            &stat_fd(&root).map_err(|error| io_refusal("ingest_path_unresolvable", path, error))?,
        );
        let mut descriptors = vec![root];
        let mut ancestry = vec![(PathBuf::from("/"), identity)];
        let mut resolved = PathBuf::from("/");
        for component in absolute.components() {
            let name = match component {
                Component::RootDir | Component::CurDir => continue,
                Component::ParentDir => {
                    // Retain the already-verified parent instead of resolving '..'
                    // against a directory that may have been moved since its open.
                    if descriptors.len() > 1 {
                        descriptors.pop();
                        ancestry.pop();
                        resolved.pop();
                    }
                    continue;
                }
                Component::Normal(name) => name,
                Component::Prefix(_) => {
                    return Err(refusal(
                        "ingest_path_unresolvable",
                        path,
                        "unsupported path prefix",
                    ))
                }
            };
            resolved.push(name);
            let parent = descriptors.last().expect("root descriptor retained");
            let checked = stat_at(parent, name)
                .map_err(|error| io_refusal("ingest_path_unresolvable", &resolved, error))?;
            if !check_type(&checked, &resolved)? {
                return Err(refusal(
                    "ingest_path_unresolvable",
                    &resolved,
                    "not a directory",
                ));
            }
            let opened = open_checked(parent, name, &resolved, &checked, before_open)?;
            ancestry.push((resolved.clone(), Identity::from(&checked)));
            descriptors.push(opened);
        }
        Ok(Directory {
            file: descriptors.pop().expect("root descriptor retained"),
            path: resolved,
            ancestry,
        })
    }

    fn names(directory: &File, path: &Path) -> Result<Vec<OsString>, AdmissionError> {
        let checked = stat_fd(directory)
            .map_err(|error| io_refusal("ingest_path_unresolvable", path, error))?;
        // Reopen '.' for an independent directory position; dup would share its offset.
        let reopened = open_checked(directory, OsStr::new("."), path, &checked, &mut |_| {})?;
        list_names(&reopened).map_err(|error| io_refusal("ingest_read_failed", path, error))
    }

    fn walk(
        directory: File,
        absolute: PathBuf,
        limit: usize,
        before_open: &mut dyn FnMut(&Path),
    ) -> Result<Vec<OpenedFile>, AdmissionError> {
        struct Frame {
            directory: File,
            absolute: PathBuf,
            relative: PathBuf,
            names: std::vec::IntoIter<OsString>,
        }
        let entries = names(&directory, &absolute)?.into_iter();
        let mut stack = vec![Frame {
            directory,
            absolute,
            relative: PathBuf::new(),
            names: entries,
        }];
        let mut files = Vec::new();
        while let Some(frame) = stack.last_mut() {
            let Some(name) = frame.names.next() else {
                stack.pop();
                continue;
            };
            let path = frame.absolute.join(&name);
            let relative = frame.relative.join(&name);
            let checked = stat_at(&frame.directory, &name)
                .map_err(|error| io_refusal("ingest_path_unresolvable", &path, error))?;
            if check_type(&checked, &path)? {
                let child = open_checked(&frame.directory, &name, &path, &checked, before_open)?;
                let entries = names(&child, &path)?.into_iter();
                stack.push(Frame {
                    directory: child,
                    absolute: path,
                    relative,
                    names: entries,
                });
            } else if files.len() < limit {
                let file = open_checked(&frame.directory, &name, &path, &checked, before_open)?;
                files.push(OpenedFile { relative, file });
            }
        }
        Ok(files)
    }

    pub(super) fn open_files(
        cfg: &WebSectionConfig,
        source: &Path,
        limit: usize,
        before_open: &mut dyn FnMut(&Path),
    ) -> Result<Vec<OpenedFile>, RuntimeError> {
        admit_files(cfg, source, limit, before_open).map_err(RuntimeError::from)
    }

    fn admit_files(
        cfg: &WebSectionConfig,
        source: &Path,
        limit: usize,
        before_open: &mut dyn FnMut(&Path),
    ) -> Result<Vec<OpenedFile>, AdmissionError> {
        let source = open_directory(source, before_open)?;
        let mut descriptor_error = None;
        let mut contained = false;
        for root in &cfg.read_roots {
            match open_directory(Path::new(root), before_open) {
                Ok(root) => {
                    if source.ancestry.iter().any(|(path, identity)| {
                        path == &root.path
                            && *identity == root.ancestry.last().expect("root identity retained").1
                    }) {
                        contained = true;
                        break;
                    }
                }
                Err(error @ AdmissionError::DescriptorExhausted { .. }) => {
                    if descriptor_error.is_none() {
                        descriptor_error = Some(error);
                    }
                }
                Err(AdmissionError::Other(_)) => {}
            }
        }
        if !contained {
            return Err(descriptor_error.unwrap_or_else(|| {
                refusal(
                    "ingest_source_outside_read_roots",
                    &source.path,
                    "outside every configured [web] read_roots entry",
                )
            }));
        }
        walk(source.file, source.path, limit, before_open)
    }
    #[cfg(test)]
    mod descriptor_retention_tests;
    #[cfg(test)]
    mod descriptor_tests;
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;

    #[test]
    fn opened_files_retain_original_bytes_after_leaf_or_directory_replacement() {
        for directory in [false, true] {
            let tree = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            let root = tree.path().canonicalize().unwrap();
            std::fs::create_dir(root.join("unused")).unwrap();
            std::fs::create_dir(root.join("served")).unwrap();
            std::fs::write(root.join("served/page.html"), b"inside bytes").unwrap();
            std::fs::write(outside.path().join("page.html"), b"outside bytes").unwrap();
            let cfg = WebSectionConfig {
                read_roots: vec![root.to_str().unwrap().to_owned()],
                ..Default::default()
            };
            let mut files =
                open_files(&cfg, &root.join("unused/../served"), 100, &mut |_| {}).unwrap();
            assert_eq!(files.len(), 1);
            assert_eq!(files[0].relative, Path::new("page.html"));
            let target = root.join(if directory {
                "served"
            } else {
                "served/page.html"
            });
            std::fs::rename(&target, root.join("original")).unwrap();
            let replacement = if directory {
                outside.path().to_path_buf()
            } else {
                outside.path().join("page.html")
            };
            std::os::unix::fs::symlink(replacement, target).unwrap();
            assert_eq!(files.pop().unwrap().read(1024).unwrap(), b"inside bytes");
        }
    }

    #[test]
    fn descriptor_walk_preserves_sorted_limit_and_hidden_files_and_refuses_special_files() {
        let tree = tempfile::tempdir().unwrap();
        let root = tree.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("a")).unwrap();
        for path in [".hidden", "a/child.html", "a.html", "z.html"] {
            std::fs::write(root.join(path), path.as_bytes()).unwrap();
        }
        let cfg = WebSectionConfig {
            read_roots: vec![root.to_str().unwrap().to_owned()],
            ..Default::default()
        };
        // The prior walk sorted complete PathBuf values, whose ordering is
        // component-based. Keep that independent oracle, including a/a.html.
        let mut expected: Vec<_> = [".hidden", "a/child.html", "a.html", "z.html"]
            .into_iter()
            .map(PathBuf::from)
            .collect();
        expected.sort();
        for limit in [0, 1, 2, 4, 10] {
            let files = open_files(&cfg, &root, limit, &mut |_| {}).unwrap();
            assert_eq!(
                files
                    .iter()
                    .map(|file| file.relative.clone())
                    .collect::<Vec<_>>(),
                expected
                    .iter()
                    .take(limit as usize)
                    .cloned()
                    .collect::<Vec<_>>()
            );
        }
        let _socket = std::os::unix::net::UnixListener::bind(root.join("socket")).unwrap();
        let error = open_files(&cfg, &root, 0, &mut |_| {}).unwrap_err();
        assert!(
            error.to_string().contains("ingest_file_type_refused"),
            "{error}"
        );
    }
}
