use super::*;
use std::process::Command;

const CHILD_CASE: &str = "KHIVE_WEB_RETAINED_DESCRIPTOR_CHILD";
const FILE_COUNT: u32 = 128;

struct LoweredLimit(libc::rlimit);

impl LoweredLimit {
    fn install() -> Self {
        let mut old = std::mem::MaybeUninit::uninit();
        // SAFETY: old is a writable out-parameter in the joined child process.
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
        assert!(new.rlim_cur >= 24, "child needs a usable descriptor budget");
        // SAFETY: new is initialized; only the child lowers its soft limit.
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &new) }, 0);
        Self(old)
    }
}

impl Drop for LoweredLimit {
    fn drop(&mut self) {
        // SAFETY: restore the child soft limit, retaining its original hard limit.
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &self.0) }, 0);
    }
}

#[test]
fn retained_files_exhaust_budget_with_a_distinct_refusal_in_a_joined_child() {
    let output = Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--exact",
            "confinement::platform::descriptor_retention_tests::many_files_exhaustion_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_CASE, "1")
        .output()
        .expect("join descriptor-limited child");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stdout} {stderr}");
    assert!(
        stdout.contains("retained-descriptor-witness:128:"),
        "child must execute the many-file case: {stdout}"
    );
}

#[test]
fn many_files_exhaustion_child() {
    if std::env::var(CHILD_CASE).as_deref() != Ok("1") {
        return;
    }
    let tree = tempfile::tempdir().expect("tree");
    let root = tree.path().canonicalize().expect("physical root");
    for index in 0..FILE_COUNT {
        std::fs::write(root.join(format!("page-{index:03}.html")), b"body")
            .expect("seed regular file before lowering the limit");
    }
    let cfg = WebSectionConfig {
        read_roots: vec![root.display().to_string()],
        ..Default::default()
    };
    let limit = LoweredLimit::install();
    let mut attempted_files = 0;
    let error = open_files(&cfg, &root, FILE_COUNT as usize, &mut |path| {
        if path.parent() == Some(root.as_path()) {
            attempted_files += 1;
        }
    })
    .expect_err("retaining 128 files must exceed the child soft limit")
    .to_string();
    assert!(
        attempted_files > 1 && attempted_files < FILE_COUNT,
        "admission must retain files before refusing: {attempted_files}"
    );
    assert!(error.contains("ingest_descriptor_exhausted"), "{error}");
    assert!(!error.contains("ingest_path_changed"), "{error}");
    assert!(
        error.contains(&io::Error::from_raw_os_error(libc::EMFILE).to_string()),
        "{error}"
    );
    assert!(
        error.contains(&format!("page-{:03}.html", attempted_files - 1)),
        "refusal must identify the file that exhausted admission: {error}"
    );
    // The failed all-or-nothing admission must release its retained descriptors.
    // A bounded retry can then scan the same complete tree at the same soft limit.
    let mut files = open_files(&cfg, &root, 1, &mut |_| {})
        .expect("smaller retry after failure must fit the unchanged descriptor budget");
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].relative, Path::new("page-000.html"));
    assert_eq!(files.pop().unwrap().read(16).unwrap(), b"body");
    drop(limit);
    println!("retained-descriptor-witness:{FILE_COUNT}:{attempted_files}");
}
