use super::{collect_md_files, import_source_identity, prepare_import_file, ImportReadSource};
use tempfile::TempDir;

fn markdown(name: &str) -> String {
    format!("# {name}\n\nThis document contains enough meaningful words to satisfy atom validation while preserving its original content and root relative source identity through the actual import preparation pipeline.")
}

#[cfg(unix)]
#[test]
fn preparation_retains_discovered_parent_after_rename_and_replacement() {
    let root = TempDir::new().unwrap();
    let original = root.path().join("nested");
    std::fs::create_dir(&original).unwrap();
    let path = original.join("source.md");
    let content = markdown("Original");
    std::fs::write(&path, &content).unwrap();
    let discovery = collect_md_files(root.path()).unwrap();
    let identity = import_source_identity(root.path(), &discovery.files[0], false).unwrap();

    let moved = TempDir::new().unwrap();
    std::fs::rename(&original, moved.path().join("original")).unwrap();
    std::fs::create_dir(&original).unwrap();
    std::fs::write(&path, markdown("Replacement")).unwrap();

    let mut total = 0;
    let prepared = prepare_import_file(
        ImportReadSource::Discovered(&discovery.walk_files[0]),
        identity,
        "atom",
        &mut total,
    )
    .unwrap();
    assert_eq!(prepared.name, "Original");
    assert_eq!(prepared.atom_content, content);
    assert_eq!(prepared.source_path, "nested/source.md");
    assert_eq!(prepared.slug, "nested--source");
    assert_eq!(total, content.len() as u64);
}

#[cfg(unix)]
#[test]
fn preparation_refuses_a_discovered_leaf_swapped_to_a_symlink() {
    use std::os::unix::fs::symlink;
    let root = TempDir::new().unwrap();
    let path = root.path().join("source.md");
    std::fs::write(&path, markdown("Original")).unwrap();
    let discovery = collect_md_files(root.path()).unwrap();
    let identity = import_source_identity(root.path(), &discovery.files[0], false).unwrap();
    let outside = TempDir::new().unwrap();
    let target = outside.path().join("outside.md");
    std::fs::write(&target, markdown("Outside")).unwrap();
    std::fs::remove_file(&path).unwrap();
    symlink(&target, &path).unwrap();
    let mut total = 0;
    let error = prepare_import_file(
        ImportReadSource::Discovered(&discovery.walk_files[0]),
        identity,
        "atom",
        &mut total,
    )
    .unwrap_err();
    assert!(error
        .to_string()
        .contains("failed to open import source \"source.md\""));
    assert_eq!(total, 0);
}

#[cfg(unix)]
#[test]
fn discovered_fifo_swap_is_refused_without_waiting_for_a_writer() {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    const CHILD: &str = "KHIVE_KNOWLEDGE_FIFO_REFUSAL_CHILD";
    if std::env::var_os(CHILD).is_none() {
        // A removed NONBLOCK flag must fail this control rather than hang the suite.
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "knowledge::sections::import_fs_tests::discovered_fifo_swap_is_refused_without_waiting_for_a_writer",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if child.try_wait().unwrap().is_some() {
                let output = child.wait_with_output().unwrap();
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                assert!(
                    String::from_utf8_lossy(&output.stdout).contains("1 passed"),
                    "child control must actually execute"
                );
                return;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("discovered FIFO admission waited for a writer");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    let root = TempDir::new().unwrap();
    let path = root.path().join("source.md");
    std::fs::write(&path, markdown("Original")).unwrap();
    let discovery = collect_md_files(root.path()).unwrap();
    let identity = import_source_identity(root.path(), &discovery.files[0], false).unwrap();
    std::fs::remove_file(&path).unwrap();
    assert!(Command::new("mkfifo")
        .arg(&path)
        .status()
        .unwrap()
        .success());
    let mut total = 0;
    let error = prepare_import_file(
        ImportReadSource::Discovered(&discovery.walk_files[0]),
        identity,
        "atom",
        &mut total,
    )
    .unwrap_err();
    assert!(error
        .to_string()
        .contains("not a regular file on the opened handle"));
    assert_eq!(total, 0);
}
