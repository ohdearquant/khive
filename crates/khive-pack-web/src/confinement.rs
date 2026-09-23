//! ADR-191 A1.1: admit and read disk entries through the same opened descriptors.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use khive_runtime::{engine_config::WebSectionConfig, RuntimeError};

use crate::egress::Refusal;

#[derive(Debug)]
pub(crate) struct OpenedFile {
    pub relative: PathBuf,
    file: File,
}

impl OpenedFile {
    pub fn read(mut self) -> Result<Vec<u8>, RuntimeError> {
        let mut bytes = Vec::new();
        self.file
            .read_to_end(&mut bytes)
            .map_err(|error| refusal("ingest_read_failed", &self.relative, error))?;
        Ok(bytes)
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
    use std::ffi::{CStr, CString, OsStr, OsString};
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::Component;

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

    fn c_name(name: &OsStr) -> io::Result<CString> {
        CString::new(name.as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path component"))
    }

    fn stat_fd(file: &File) -> io::Result<libc::stat> {
        let mut stat = std::mem::MaybeUninit::uninit();
        // SAFETY: file owns a live descriptor and stat is a writable out-parameter.
        if unsafe { libc::fstat(file.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful fstat initialized the entire structure.
        Ok(unsafe { stat.assume_init() })
    }

    fn stat_at(parent: &File, name: &OsStr) -> io::Result<libc::stat> {
        let name = c_name(name)?;
        let mut stat = std::mem::MaybeUninit::uninit();
        // SAFETY: parent is live, name is terminated, and stat is writable.
        if unsafe {
            libc::fstatat(
                parent.as_raw_fd(),
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful fstatat initialized the structure.
        Ok(unsafe { stat.assume_init() })
    }

    fn check_type(stat: &libc::stat, path: &Path) -> Result<bool, RuntimeError> {
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
    ) -> Result<File, RuntimeError> {
        let directory = check_type(checked, path)?;
        let name =
            c_name(name).map_err(|error| refusal("ingest_path_unresolvable", path, error))?;
        before_open(path);
        // O_NONBLOCK prevents a raced-in FIFO from blocking before fstat rejects it.
        let flags = libc::O_RDONLY
            | libc::O_CLOEXEC
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | if directory { libc::O_DIRECTORY } else { 0 };
        // SAFETY: parent is live and name contains exactly one terminated component.
        let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(refusal(
                "ingest_path_changed",
                path,
                io::Error::last_os_error(),
            ));
        }
        // SAFETY: successful openat returned a uniquely owned descriptor.
        let opened = unsafe { File::from_raw_fd(fd) };
        let actual =
            stat_fd(&opened).map_err(|error| refusal("ingest_path_unresolvable", path, error))?;
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
    ) -> Result<Directory, RuntimeError> {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map_err(|error| refusal("ingest_path_unresolvable", path, error))?
                .join(path)
        };
        // The filesystem root has no caller-controlled ancestor to follow.
        let root = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open("/")
            .map_err(|error| refusal("ingest_path_unresolvable", path, error))?;
        let identity = Identity::from(
            &stat_fd(&root).map_err(|error| refusal("ingest_path_unresolvable", path, error))?,
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
                .map_err(|error| refusal("ingest_path_unresolvable", &resolved, error))?;
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

    struct DirStream(*mut libc::DIR);

    impl Drop for DirStream {
        fn drop(&mut self) {
            // SAFETY: this wrapper uniquely owns the successful fdopendir result.
            unsafe { libc::closedir(self.0) };
        }
    }

    #[cfg(target_os = "macos")]
    fn errno_location() -> *mut libc::c_int {
        // SAFETY: the accessor returns the current thread's live errno cell.
        unsafe { libc::__error() }
    }

    #[cfg(not(target_os = "macos"))]
    fn errno_location() -> *mut libc::c_int {
        // SAFETY: the accessor returns the current thread's live errno cell.
        unsafe { libc::__errno_location() }
    }

    fn names(directory: &File, path: &Path) -> Result<Vec<OsString>, RuntimeError> {
        let checked =
            stat_fd(directory).map_err(|error| refusal("ingest_path_unresolvable", path, error))?;
        // Reopen '.' for an independent directory position; dup would share its offset.
        let reopened = open_checked(directory, OsStr::new("."), path, &checked, &mut |_| {})?;
        let fd = reopened.into_raw_fd();
        // SAFETY: fd is uniquely owned and fdopendir takes ownership on success.
        let stream = unsafe { libc::fdopendir(fd) };
        if stream.is_null() {
            let error = io::Error::last_os_error();
            // SAFETY: fdopendir failed, so ownership of fd remains here.
            unsafe { libc::close(fd) };
            return Err(refusal("ingest_read_failed", path, error));
        }
        let stream = DirStream(stream);
        let mut result = Vec::new();
        loop {
            // SAFETY: errno is thread-local; stream remains live until this function returns.
            unsafe { *errno_location() = 0 };
            let entry = unsafe { libc::readdir(stream.0) };
            if entry.is_null() {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(0) {
                    return Err(refusal("ingest_read_failed", path, error));
                }
                break;
            }
            // SAFETY: d_name is terminated and copied before the next readdir call.
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
            if name != b"." && name != b".." {
                result.push(OsString::from_vec(name.to_vec()));
            }
        }
        result.sort();
        Ok(result)
    }

    fn walk(
        directory: File,
        absolute: PathBuf,
        limit: usize,
        before_open: &mut dyn FnMut(&Path),
    ) -> Result<Vec<OpenedFile>, RuntimeError> {
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
                .map_err(|error| refusal("ingest_path_unresolvable", &path, error))?;
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
        let source = open_directory(source, before_open)?;
        let contained = cfg.read_roots.iter().any(|root| {
            open_directory(Path::new(root), before_open)
                .map(|root| {
                    source.ancestry.iter().any(|(path, identity)| {
                        path == &root.path
                            && *identity == root.ancestry.last().expect("root identity retained").1
                    })
                })
                .unwrap_or(false)
        });
        if !contained {
            return Err(refusal(
                "ingest_source_outside_read_roots",
                &source.path,
                "outside every configured [web] read_roots entry",
            ));
        }
        walk(source.file, source.path, limit, before_open)
    }
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
            assert_eq!(files.pop().unwrap().read().unwrap(), b"inside bytes");
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
