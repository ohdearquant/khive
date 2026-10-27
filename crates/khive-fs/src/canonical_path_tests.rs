use super::canonicalize_allow_missing;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(any(windows, target_os = "linux"))]
use std::ffi::OsString;

static NEXT_SCRATCH: AtomicU64 = AtomicU64::new(0);

struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new() -> Self {
        Self::in_dir(&std::env::temp_dir())
    }

    fn in_dir(parent: &Path) -> Self {
        for _ in 0..128 {
            let tick = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("temporary fixture clock must follow UNIX_EPOCH")
                .as_nanos();
            let sequence = NEXT_SCRATCH.fetch_add(1, Ordering::Relaxed);
            let path = parent.join(format!(
                "khive-canonical-path-{}-{tick}-{sequence}",
                std::process::id()
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Self { path },
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create owned fixture {}: {error}", path.display()),
            }
        }
        panic!("could not exclusively create an owned temporary fixture");
    }

    fn root(&self) -> PathBuf {
        fs::canonicalize(&self.path).expect("canonicalize owned fixture root")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // Only a path whose exclusive create_dir succeeded is ever stored here.
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn assert_absent(path: &Path) {
    let error = fs::symlink_metadata(path).expect_err("fixture path must stay absent");
    assert_eq!(error.kind(), io::ErrorKind::NotFound, "{}", path.display());
}

#[test]
fn c1_existing_endpoints_equal_native_canonicalization() {
    let scratch = Scratch::new();
    let root = scratch.root();
    let file = root.join("source");
    let directory = root.join("directory");
    fs::write(&file, b"source bytes").unwrap();
    fs::create_dir(&directory).unwrap();
    let parent_spelling = directory.join("..").join("source");
    for input in [&file, &directory, &parent_spelling] {
        assert_eq!(
            canonicalize_allow_missing(input).unwrap(),
            fs::canonicalize(input).unwrap()
        );
    }
    assert_eq!(fs::read(&file).unwrap(), b"source bytes");
}

#[test]
#[cfg(unix)]
fn c1_existing_symlinks_equal_native_canonicalization() {
    use std::os::unix::fs::symlink;

    let scratch = Scratch::new();
    let root = scratch.root();
    let file = root.join("source");
    let directory = root.join("directory");
    fs::write(&file, b"source bytes").unwrap();
    fs::create_dir(&directory).unwrap();
    for (name, target) in [("file-link", &file), ("directory-link", &directory)] {
        let link = root.join(name);
        symlink(target, &link).unwrap();
        assert_eq!(
            canonicalize_allow_missing(&link).unwrap(),
            fs::canonicalize(&link).unwrap()
        );
        assert_eq!(fs::read_link(&link).unwrap(), *target);
    }
    assert_eq!(fs::read(&file).unwrap(), b"source bytes");
}

#[test]
fn c2_fresh_nested_tail_is_not_created() {
    let scratch = Scratch::new();
    let root = scratch.root();
    let input = root.join("fresh").join("deep").join("leaf");
    assert_eq!(
        fs::canonicalize(&input).unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
    assert_eq!(canonicalize_allow_missing(&input).unwrap(), input);
    assert_absent(&root.join("fresh"));
}

#[test]
fn c2_relative_fresh_tail_uses_cwd_without_chdir() {
    let cwd = std::env::current_dir().unwrap();
    let scratch = Scratch::in_dir(&cwd);
    let input = PathBuf::from(scratch.path.file_name().unwrap())
        .join("fresh")
        .join("leaf");
    assert!(!input.is_absolute());
    assert_eq!(
        canonicalize_allow_missing(&input).unwrap(),
        scratch.root().join("fresh").join("leaf")
    );
    assert_absent(&scratch.path.join("fresh"));
    assert_eq!(std::env::current_dir().unwrap(), cwd);
}

#[test]
#[cfg(unix)]
fn c3_existing_directory_links_keep_fresh_tail() {
    use std::os::unix::fs::symlink;

    let scratch = Scratch::new();
    let root = scratch.root();
    let real = root.join("real");
    fs::create_dir(&real).unwrap();
    for (name, target) in [
        ("absolute-link", real.clone()),
        ("relative-link", PathBuf::from("real")),
    ] {
        let link = root.join(name);
        symlink(&target, &link).unwrap();
        assert_eq!(
            canonicalize_allow_missing(&link.join("fresh").join("leaf")).unwrap(),
            real.join("fresh").join("leaf")
        );
        assert_eq!(fs::read_link(&link).unwrap(), target);
    }
    assert_absent(&real.join("fresh"));
}

#[test]
#[cfg(unix)]
fn c3_multicomponent_relative_target_uses_link_parent() {
    use std::os::unix::fs::symlink;

    let scratch = Scratch::new();
    let root = scratch.root();
    let child = root.join("child");
    let real = root.join("real");
    fs::create_dir(&child).unwrap();
    fs::create_dir(&real).unwrap();
    let target = Path::new("..").join("real");
    let link = child.join("link");
    symlink(&target, &link).unwrap();
    let input = link.join("fresh").join("leaf");
    assert_eq!(
        fs::canonicalize(&input).unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
    assert_eq!(
        canonicalize_allow_missing(&input).unwrap(),
        real.join("fresh").join("leaf")
    );
    assert_eq!(fs::read_link(&link).unwrap(), target);
    assert_absent(&real.join("fresh"));
    assert_absent(&child.join("fresh"));
}

#[test]
#[cfg(unix)]
fn c4_canceled_missing_parent_reinspects_symlink() {
    use std::os::unix::fs::symlink;

    let scratch = Scratch::new();
    let root = scratch.root();
    let real = root.join("real");
    fs::create_dir(&real).unwrap();
    let link = root.join("alias");
    symlink("real", &link).unwrap();
    let input = root.join("missing").join("..").join("alias").join("fresh");
    assert_eq!(
        fs::canonicalize(&input).unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
    assert_eq!(
        canonicalize_allow_missing(&input).unwrap(),
        real.join("fresh")
    );
    assert_absent(&root.join("missing"));
    assert_absent(&real.join("fresh"));
    assert_eq!(fs::read_link(&link).unwrap(), Path::new("real"));
}

#[test]
#[cfg(unix)]
fn c5_dangling_links_normalize_missing_targets() {
    use std::os::unix::fs::symlink;

    let scratch = Scratch::new();
    let root = scratch.root();
    let target = root.join("fresh-dir");
    let relative_target = Path::new("not-created").join("..").join("fresh-dir");
    let relative = root.join("relative");
    let absolute = root.join("absolute");
    symlink(&relative_target, &relative).unwrap();
    symlink(&target, &absolute).unwrap();
    for link in [&relative, &absolute] {
        assert_eq!(canonicalize_allow_missing(link).unwrap(), target);
        assert_eq!(
            canonicalize_allow_missing(&link.join("leaf")).unwrap(),
            target.join("leaf")
        );
    }
    assert_eq!(fs::read_link(&relative).unwrap(), relative_target);
    assert_eq!(fs::read_link(&absolute).unwrap(), target);
    let missing_file = root.join("fresh.db");
    for (name, link_target) in [
        ("relative-file", PathBuf::from("fresh.db")),
        ("absolute-file", missing_file.clone()),
    ] {
        let link = root.join(name);
        symlink(&link_target, &link).unwrap();
        assert_eq!(canonicalize_allow_missing(&link).unwrap(), missing_file);
        assert_eq!(fs::read_link(&link).unwrap(), link_target);
    }
    assert_absent(&root.join("not-created"));
    assert_absent(&target);
    assert_absent(&missing_file);
}

#[test]
fn c6_nondirectory_child_keeps_native_error() {
    let scratch = Scratch::new();
    let file = scratch.root().join("file");
    fs::write(&file, b"file bytes").unwrap();
    let input = file.join("child");
    let expected = fs::canonicalize(&input).unwrap_err();
    assert_eq!(
        canonicalize_allow_missing(&input).unwrap_err().kind(),
        expected.kind()
    );
    assert_eq!(fs::read(&file).unwrap(), b"file bytes");
}

#[test]
#[cfg(unix)]
fn c6_parent_cannot_erase_nondirectory_traversal() {
    let scratch = Scratch::new();
    let root = scratch.root();
    let file = root.join("file");
    fs::write(&file, b"file bytes").unwrap();
    let input = file.join("..").join("leaf");
    let expected = fs::canonicalize(&input).unwrap_err();
    assert_eq!(
        canonicalize_allow_missing(&input).unwrap_err().kind(),
        expected.kind()
    );
    assert_eq!(fs::read(&file).unwrap(), b"file bytes");
    assert_absent(&root.join("leaf"));
}

#[test]
#[cfg(unix)]
fn c6_fallback_nondirectory_child_keeps_native_error() {
    let scratch = Scratch::new();
    let root = scratch.root();
    let file = root.join("file");
    fs::write(&file, b"file bytes").unwrap();
    for suffix in [PathBuf::from("child"), Path::new("..").join("leaf")] {
        let input = root.join("missing").join("..").join("file").join(&suffix);
        assert_eq!(
            fs::canonicalize(&input).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        let native = fs::canonicalize(file.join(&suffix)).unwrap_err();
        let actual = canonicalize_allow_missing(&input).unwrap_err();
        assert_eq!(actual.kind(), native.kind());
        assert_eq!(actual.raw_os_error(), native.raw_os_error());
    }
    assert_eq!(fs::read(&file).unwrap(), b"file bytes");
    assert_absent(&root.join("missing"));
    assert_absent(&root.join("leaf"));
}

#[cfg(unix)]
fn link_chain(root: &Path, prefix: &str, count: usize, end: &str) -> Vec<(PathBuf, PathBuf)> {
    use std::os::unix::fs::symlink;

    let mut links = Vec::new();
    for index in 0..count {
        let link = root.join(format!("{prefix}{index:02}"));
        let target = if index + 1 == count {
            PathBuf::from(end)
        } else {
            PathBuf::from(format!("{prefix}{:02}", index + 1))
        };
        symlink(&target, &link).unwrap();
        links.push((link, target));
    }
    links
}

#[cfg(unix)]
fn assert_chain_unchanged(links: &[(PathBuf, PathBuf)]) {
    for (link, target) in links {
        assert_eq!(fs::read_link(link).unwrap(), *target);
    }
}

#[test]
#[cfg(unix)]
fn c8_fallback_accepts_exactly_forty_finite_links() {
    let scratch = Scratch::new();
    let root = scratch.root();
    let links = link_chain(&root, "link", 40, "fresh-target");
    let input = root
        .join("missing")
        .join("..")
        .join(links[0].0.file_name().unwrap());
    assert_eq!(
        fs::canonicalize(&input).unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
    assert_eq!(
        canonicalize_allow_missing(&input).unwrap(),
        root.join("fresh-target")
    );
    assert_absent(&root.join("missing"));
    assert_absent(&root.join("fresh-target"));
    assert_chain_unchanged(&links);
}

#[test]
#[cfg(unix)]
fn c8_fallback_refuses_forty_first_finite_link() {
    let scratch = Scratch::new();
    let root = scratch.root();
    let links = link_chain(&root, "link", 41, "fresh-target");
    let input = root
        .join("missing")
        .join("..")
        .join(links[0].0.file_name().unwrap());
    assert_eq!(
        fs::canonicalize(&input).unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
    let error = canonicalize_allow_missing(&input).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    assert_eq!(
        error.to_string(),
        "too many symlinks while resolving export destination"
    );
    assert_absent(&root.join("missing"));
    assert_absent(&root.join("fresh-target"));
    assert_chain_unchanged(&links);
}

#[test]
#[cfg(unix)]
fn c8_existing_and_fallback_cycles_keep_distinct_errors() {
    use std::os::unix::fs::symlink;

    let scratch = Scratch::new();
    let root = scratch.root();
    let first = root.join("first");
    let second = root.join("second");
    symlink("second", &first).unwrap();
    symlink("first", &second).unwrap();
    let native = fs::canonicalize(&first).unwrap_err();
    assert_eq!(
        canonicalize_allow_missing(&first).unwrap_err().kind(),
        native.kind()
    );
    let fallback = root.join("missing").join("..").join("first");
    assert_eq!(
        fs::canonicalize(&fallback).unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
    let error = canonicalize_allow_missing(&fallback).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    assert_eq!(
        error.to_string(),
        "too many symlinks while resolving export destination"
    );
    assert_eq!(fs::read_link(&first).unwrap(), Path::new("second"));
    assert_eq!(fs::read_link(&second).unwrap(), Path::new("first"));
    assert_absent(&root.join("missing"));
    // A cap-removal timeout is not semantic mutation-kill evidence.
}

#[cfg(windows)]
fn current_disk() -> (PathBuf, u8) {
    use std::path::{Component, Prefix};

    let cwd = std::env::current_dir().unwrap();
    let disk = match cwd.components().next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::Disk(disk) | Prefix::VerbatimDisk(disk) => disk,
            other => panic!("fixture requires an actual disk-backed CWD, found {other:?}"),
        },
        other => panic!("fixture requires an actual disk-backed CWD, found {other:?}"),
    };
    (cwd, disk)
}

#[test]
#[cfg(windows)]
fn c9_windows_rooted_and_root_relative_missing_paths() {
    use std::path::Component;

    let (cwd, _) = current_disk();
    let scratch = Scratch::in_dir(&cwd);
    let input = scratch.root().join("fresh").join("leaf");
    assert!(input.is_absolute());
    assert_eq!(
        fs::canonicalize(&input).unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
    assert_eq!(canonicalize_allow_missing(&input).unwrap(), input);
    let mut components = input.components();
    assert!(matches!(components.next(), Some(Component::Prefix(_))));
    let root_relative = components.as_path().to_path_buf();
    assert!(root_relative.has_root());
    assert!(!root_relative.is_absolute());
    assert!(cwd.join(&root_relative).is_absolute());
    assert_eq!(canonicalize_allow_missing(&root_relative).unwrap(), input);
    assert_absent(&scratch.path.join("fresh"));
    assert_eq!(std::env::current_dir().unwrap(), cwd);
}

#[test]
#[cfg(windows)]
fn c9_windows_missing_drive_relative_is_refused() {
    let (cwd, disk) = current_disk();
    let scratch = Scratch::in_dir(&cwd);
    let mut spelling = OsString::from(format!("{}:", char::from(disk)));
    spelling.push(scratch.path.file_name().unwrap());
    spelling.push("\\fresh\\leaf");
    let input = PathBuf::from(spelling);
    assert!(!input.is_absolute());
    assert_eq!(
        fs::canonicalize(&input).unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
    assert!(!cwd.join(&input).is_absolute());
    let error = canonicalize_allow_missing(&input).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    assert_eq!(
        error.to_string(),
        "export destination must resolve to a fully rooted path"
    );
    assert_absent(&scratch.path.join("fresh"));
    assert_eq!(std::env::current_dir().unwrap(), cwd);
}

#[test]
#[cfg(windows)]
fn c10_windows_existing_drive_relative_uses_fast_path() {
    let (cwd, disk) = current_disk();
    let input = PathBuf::from(format!("{}:.", char::from(disk)));
    assert!(!input.is_absolute());
    let expected = fs::canonicalize(&input).expect("current-drive dot must exist");
    assert_eq!(canonicalize_allow_missing(&input).unwrap(), expected);
    assert_eq!(std::env::current_dir().unwrap(), cwd);
}

#[test]
#[cfg(target_os = "linux")]
fn c11_linux_missing_tail_preserves_non_utf8_bytes() {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    let scratch = Scratch::new();
    let root = scratch.root();
    let native_name = OsString::from_vec(vec![b'f', b'r', b'e', b's', b'h', 0xff]);
    let fresh = root.join(&native_name);
    let input = fresh.join("leaf");
    let resolved = canonicalize_allow_missing(&input).unwrap();
    assert_eq!(
        resolved.as_os_str().as_bytes(),
        input.as_os_str().as_bytes()
    );
    assert_absent(&fresh);
}
