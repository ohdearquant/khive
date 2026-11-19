use super::{read_import_file, MAX_IMPORT_FILE_BYTES, MAX_IMPORT_TOTAL_BYTES};
use tempfile::TempDir;

#[cfg(unix)]
#[test]
fn symlink_swap_at_read_time_is_rejected() {
    use std::os::unix::fs::symlink;

    let root = TempDir::new().expect("temp root");
    let outside = TempDir::new().expect("outside root");
    let secret = outside.path().join("secret.md");
    std::fs::write(&secret, "outside content").expect("secret fixture");

    // Simulates the post-validation swap: the path traversal accepted
    // as a regular file is, by read time, a symlink pointing outside
    // the import root.
    let swapped = root.path().join("swapped.md");
    symlink(&secret, &swapped).expect("swap symlink fixture");

    let mut total_bytes = 0u64;
    let error = read_import_file(&swapped, "swapped.md", &mut total_bytes)
        .expect_err("a symlink swapped in after validation must be refused at read time");
    assert!(
        error.to_string().contains("failed to open import source"),
        "{error}"
    );
    assert_eq!(
        total_bytes, 0,
        "a rejected read must not count toward the aggregate cap"
    );
}

#[test]
fn oversize_single_file_is_refused() {
    let root = TempDir::new().expect("temp root");
    let big = root.path().join("big.md");
    let file = std::fs::File::create(&big).expect("create big fixture");
    file.set_len(MAX_IMPORT_FILE_BYTES + 1)
        .expect("grow big fixture past the per-file cap");
    drop(file);

    let mut total_bytes = 0u64;
    let error = read_import_file(&big, "big.md", &mut total_bytes)
        .expect_err("a file over the per-file cap must be refused");
    assert!(
        error.to_string().contains("exceeding the per-file cap"),
        "{error}"
    );
    assert_eq!(
        total_bytes, 0,
        "a rejected read must not count toward the aggregate cap"
    );
}

#[test]
fn aggregate_cap_is_refused_once_tripped() {
    let root = TempDir::new().expect("temp root");
    let small = root.path().join("small.md");
    std::fs::write(
        &small,
        "a small file that pushes the running total over the cap",
    )
    .expect("small fixture");

    // Seed the running total just under the aggregate cap so this one
    // small read is what tips it over, without needing to write a
    // 256 MiB fixture to disk.
    let mut total_bytes = MAX_IMPORT_TOTAL_BYTES - 5;
    let error = read_import_file(&small, "small.md", &mut total_bytes)
        .expect_err("a read that crosses the aggregate cap must be refused");
    assert!(
        error.to_string().contains("exceeds the total-import cap"),
        "{error}"
    );
}

#[test]
fn clean_read_under_both_caps_succeeds() {
    let root = TempDir::new().expect("temp root");
    let clean = root.path().join("clean.md");
    std::fs::write(&clean, "# Clean\n\nWell under every cap.").expect("clean fixture");

    let mut total_bytes = 0u64;
    let content = read_import_file(&clean, "clean.md", &mut total_bytes)
        .expect("a file under both caps must read successfully");
    assert_eq!(content, "# Clean\n\nWell under every cap.");
    assert_eq!(total_bytes, content.len() as u64);
}
