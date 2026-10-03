use super::*;
use std::fs::File;
use std::path::PathBuf;
use std::process::Command;

const CHILD: &str = "KHIVE_WEB_DISK_DESCRIPTOR_CHILD";

#[test]
fn disk_descriptor_refusal_child_is_joined_and_writes_nothing() {
    let output = Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--exact",
            "ingest::tests::descriptor_tests::disk_descriptor_refusal_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD, "1")
        .output()
        .expect("join isolated disk admission child");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stdout} {stderr}");
    assert!(
        stdout.contains("disk-descriptor-witness:no-writes"),
        "must execute the child: {stdout}"
    );
}

#[tokio::test]
async fn disk_descriptor_refusal_child() {
    if std::env::var(CHILD).as_deref() != Ok("1") {
        return;
    }
    let tree = tempfile::tempdir().expect("tree");
    let root = PathBuf::from(disk_path(tree.path()));
    let leaf = root.join("page.html");
    std::fs::write(&leaf, b"<html>body</html>").unwrap();
    let (runtime, token, db_dir) =
        test_runtime_with_read_roots(vec![root.display().to_string()]).await;
    let pack = WebPack::new(runtime.clone());
    assert_no_records(&runtime, &token).await;
    let blob_entries = || {
        std::fs::read_dir(db_dir.path().join("blobs"))
            .unwrap()
            .count()
    };
    let before_blobs = blob_entries();
    let mut old = std::mem::MaybeUninit::uninit();
    // SAFETY: old is writable and initialized only after successful getrlimit.
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, old.as_mut_ptr()) },
        0
    );
    // SAFETY: successful getrlimit initialized old.
    let old = unsafe { old.assume_init() };
    let limited = libc::rlimit {
        rlim_cur: old.rlim_cur.min(64),
        rlim_max: old.rlim_max,
    };
    assert!(limited.rlim_cur >= 24);
    // SAFETY: only this isolated, joined child modifies its soft descriptor limit.
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limited) }, 0);
    let mut held = Vec::new();
    let mut observed = false;
    let result = ingest_disk_before_open(
        &pack,
        &token,
        &runtime.config().web,
        root.to_str().unwrap(),
        "https://descriptor.example.test",
        100,
        &mut |path| {
            if path == leaf {
                observed = true;
                loop {
                    match File::open("/dev/null") {
                        Ok(file) => held.push(file),
                        Err(error) => {
                            assert_eq!(
                                error.raw_os_error(),
                                Some(libc::EMFILE),
                                "real exhaustion premise"
                            );
                            break;
                        }
                    }
                }
            }
        },
    )
    .await;
    drop(held);
    // SAFETY: restore this child's soft limit before SQLite/no-mutation verification.
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &old) }, 0);
    assert!(observed, "actual disk admission must reach the file open");
    let error = result
        .expect_err("actual disk admission returns descriptor refusal")
        .to_string();
    assert!(error.contains("ingest_descriptor_exhausted"), "{error}");
    assert_no_records(&runtime, &token).await;
    assert_eq!(
        blob_entries(),
        before_blobs,
        "no body blob written on admission failure"
    );
    println!("disk-descriptor-witness:no-writes");
}
