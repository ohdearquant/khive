use super::*;
use std::process::Command;

const CHILD_CASE: &str = "KHIVE_WEB_DESCRIPTOR_CHILD_CASE";

struct LoweredLimit(libc::rlimit);

impl LoweredLimit {
    fn install() -> Self {
        let mut old = std::mem::MaybeUninit::uninit();
        // SAFETY: old is a writable out-parameter for this child process.
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, old.as_mut_ptr()) },
            0
        );
        // SAFETY: successful getrlimit initialized old.
        let old = unsafe { old.assume_init() };
        let new = libc::rlimit {
            rlim_cur: old.rlim_cur.min(64),
            rlim_max: old.rlim_max,
        };
        assert!(
            new.rlim_cur >= 24,
            "child needs a usable initial descriptor budget"
        );
        // SAFETY: new is initialized; only this joined child lowers its soft limit.
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &new) }, 0);
        Self(old)
    }
}

impl Drop for LoweredLimit {
    fn drop(&mut self) {
        // SAFETY: restore the child soft limit without changing its hard limit.
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &self.0) }, 0);
    }
}

fn exhaust() -> Vec<File> {
    let mut held = Vec::new();
    loop {
        match File::open("/dev/null") {
            Ok(file) => held.push(file),
            Err(error) => {
                assert_eq!(
                    error.raw_os_error(),
                    Some(libc::EMFILE),
                    "real child exhaustion"
                );
                return held;
            }
        }
    }
}

#[test]
fn descriptor_errno_classifier_keeps_raw_causes_and_other_refusals() {
    let path = Path::new("raw-cause");
    for fallback in [
        "ingest_path_changed",
        "ingest_path_unresolvable",
        "ingest_read_failed",
    ] {
        for errno in [libc::EMFILE, libc::ENFILE] {
            let error = io_refusal(fallback, path, io::Error::from_raw_os_error(errno));
            match &error {
                AdmissionError::DescriptorExhausted {
                    path: observed,
                    error,
                } => {
                    assert_eq!(observed, path);
                    assert_eq!(error.raw_os_error(), Some(errno));
                }
                AdmissionError::Other(_) => panic!("raw errno must retain its descriptor class"),
            }
            let rendered = RuntimeError::from(error).to_string();
            assert!(
                rendered.contains("ingest_descriptor_exhausted"),
                "{rendered}"
            );
            assert!(rendered.contains("raw-cause"), "{rendered}");
            assert!(rendered.contains(&io::Error::from_raw_os_error(errno).to_string()));
        }
        for error in [
            io::Error::from_raw_os_error(libc::ENOENT),
            io::Error::from_raw_os_error(libc::EACCES),
            io::Error::other("Too many open files (os error 24)"),
        ] {
            let expected = super::super::refusal(fallback, path, &error).to_string();
            assert_eq!(
                RuntimeError::from(io_refusal(fallback, path, error)).to_string(),
                expected
            );
        }
    }
}

#[test]
fn descriptor_admission_runs_with_real_emfile_in_joined_children() {
    for case in [
        "root-open",
        "source-openat",
        "directory-names",
        "walk-directory",
        "walk-file",
        "root-refusal",
        "first-root-refusal",
        "later-root-admits",
    ] {
        let output = Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "confinement::platform::descriptor_tests::descriptor_exhaustion_child",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD_CASE, case)
            .output()
            .expect("join descriptor-limited child");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{case}: {stdout} {stderr}");
        assert!(
            stdout.contains(&format!("descriptor-witness:{case}")),
            "child must execute, not ran0: {stdout}"
        );
    }
}

#[test]
fn descriptor_exhaustion_child() {
    let Ok(case) = std::env::var(CHILD_CASE) else {
        return;
    };
    let tree = tempfile::tempdir().expect("tree");
    let root = tree.path().canonicalize().expect("physical root");
    let source = root.join("served");
    let first = root.join("blocked-first");
    let second = root.join("blocked-second");
    for path in [&source, &first, &second, &source.join("sub")] {
        std::fs::create_dir(path).expect("directory");
    }
    std::fs::write(source.join("page.html"), b"body").expect("file");
    let cfg = WebSectionConfig {
        read_roots: if case == "first-root-refusal" {
            vec![first.display().to_string(), second.display().to_string()]
        } else if case == "later-root-admits" {
            vec![first.display().to_string(), root.display().to_string()]
        } else if case == "root-refusal" {
            vec![first.display().to_string()]
        } else {
            vec![root.display().to_string()]
        },
        ..Default::default()
    };
    let directory =
        (case == "directory-names").then(|| File::open(&source).expect("opened directory"));
    let limit = LoweredLimit::install();
    let mut held = Vec::new();
    if case == "root-open" || case == "directory-names" {
        held = exhaust();
    }
    let result = if let Some(directory) = directory {
        names(&directory, &source)
            .map(|_| Vec::<OpenedFile>::new())
            .map_err(RuntimeError::from)
    } else {
        let mut refused_root = false;
        open_files(&cfg, &source, 100, &mut |path| {
            if refused_root && (case == "later-root-admits" || case == "first-root-refusal") {
                held.clear();
                refused_root = false;
            }
            let target = match case.as_str() {
                "source-openat" => path == source,
                "walk-directory" => path == source.join("sub"),
                "walk-file" => path == source.join("page.html"),
                "root-refusal" | "later-root-admits" => path == first,
                "first-root-refusal" => path == first || path == second,
                _ => false,
            };
            if target {
                assert!(held.is_empty());
                held = exhaust();
                refused_root = true;
            }
        })
    };
    held.clear();
    drop(limit);
    if case == "later-root-admits" {
        let mut files = result.expect("later valid root must win over earlier descriptor refusal");
        assert_eq!(files.len(), 1);
        assert_eq!(files.pop().unwrap().read(16).unwrap(), b"body");
    } else {
        let error = result
            .expect_err("shipping admission must observe real EMFILE")
            .to_string();
        assert!(
            error.contains("ingest_descriptor_exhausted"),
            "{case}: {error}"
        );
        assert!(
            error.contains(&io::Error::from_raw_os_error(libc::EMFILE).to_string()),
            "{error}"
        );
        if case == "root-refusal" || case == "first-root-refusal" {
            assert!(
                error.contains("blocked-first"),
                "first raw root refusal is retained: {error}"
            );
            assert!(
                !error.contains("blocked-second"),
                "later root cannot overwrite the first: {error}"
            );
        }
    }
    println!("descriptor-witness:{case}");
}

#[test]
fn unrelated_root_failures_keep_any_root_and_outside_root_behavior() {
    let tree = tempfile::tempdir().unwrap();
    let root = tree.path().canonicalize().unwrap();
    let unrelated = tempfile::tempdir().unwrap();
    let missing = root.join("missing");
    std::fs::write(root.join("page"), b"body").unwrap();
    let mut cfg = WebSectionConfig {
        read_roots: vec![
            missing.display().to_string(),
            unrelated
                .path()
                .canonicalize()
                .unwrap()
                .display()
                .to_string(),
        ],
        ..Default::default()
    };
    let error = open_files(&cfg, &root, 100, &mut |_| {})
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("ingest_source_outside_read_roots"),
        "{error}"
    );
    cfg.read_roots.push(root.display().to_string());
    assert_eq!(open_files(&cfg, &root, 100, &mut |_| {}).unwrap().len(), 1);
}
