use std::fs::File;
use std::io;
use std::path::Path;

use khive_fs::opened_file::{open_regular_file_within, ContainedOpenError};

use khive_runtime::bounded_read::read_to_end_bounded;

/// Shared L1 manifest, L1.5 source, and L2 Rust source admission ceiling.
pub(crate) const MAX_INGEST_FILE_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub(crate) enum SourceReadError {
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error("{0}")]
    Refused(String),
}

pub(crate) fn open_contained_file(
    canonical_root: &Path,
    source_path: &Path,
) -> Result<File, SourceReadError> {
    open_regular_file_within(canonical_root, source_path)
        .map_err(|error| contained_open_refusal(source_path, error))
}

/// The refusal text this pack reports for each way the shared contained open can refuse.
fn contained_open_refusal(source_path: &Path, error: ContainedOpenError) -> SourceReadError {
    match error {
        ContainedOpenError::Open(error) => SourceReadError::Refused(format!(
            "cannot open candidate source {}: {error}",
            source_path.display()
        )),
        ContainedOpenError::Metadata(error) => SourceReadError::Refused(format!(
            "cannot verify opened source type {}: {error}",
            source_path.display()
        )),
        ContainedOpenError::NotRegular => SourceReadError::Refused(format!(
            "opened source is not a regular file: {}",
            source_path.display()
        )),
        ContainedOpenError::Resolve(error) => SourceReadError::Refused(format!(
            "cannot verify opened source {}: {error}",
            source_path.display()
        )),
        ContainedOpenError::Escapes { opened } => SourceReadError::Refused(format!(
            "opened source escapes the canonical ingest root: {} -> {}",
            source_path.display(),
            opened.display()
        )),
    }
}

pub(crate) fn read_contained_to_string(
    canonical_root: &Path,
    source_path: &Path,
) -> Result<String, SourceReadError> {
    let source = open_contained_file(canonical_root, source_path)?;
    // Inspect the opened descriptor, then cap the read on that same handle.
    // A file growing after metadata inspection cannot allocate unboundedly.
    if source.metadata()?.len() > MAX_INGEST_FILE_BYTES {
        return Err(SourceReadError::Refused(format!(
            "file {} exceeds the {}-byte code ingest ceiling",
            source_path.display(),
            MAX_INGEST_FILE_BYTES
        )));
    }
    let Some(bytes) = read_to_end_bounded(source, MAX_INGEST_FILE_BYTES)? else {
        return Err(SourceReadError::Refused(format!(
            "file {} exceeds the {}-byte code ingest ceiling",
            source_path.display(),
            MAX_INGEST_FILE_BYTES
        )));
    };
    String::from_utf8(bytes)
        .map_err(|error| SourceReadError::Io(io::Error::new(io::ErrorKind::InvalidData, error)))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;
    use tempfile::TempDir;

    #[test]
    fn source_swapped_to_outside_symlink_after_containment_check_is_refused() {
        let fixture = TempDir::new().expect("fixture");
        let root = fixture.path().join("root");
        fs::create_dir(&root).expect("root");
        let path = root.join("lib.rs");
        fs::write(&path, "pub fn inside() {}\n").expect("inside source");
        let outside = fixture.path().join("outside.rs");
        fs::write(&outside, "pub fn outside() {}\n").expect("outside source");

        let canonical_root = root.canonicalize().expect("canonical root");
        assert!(path
            .canonicalize()
            .expect("checked path")
            .starts_with(&canonical_root));
        fs::remove_file(&path).expect("remove checked source");
        symlink(&outside, &path).expect("swap to outside symlink");

        assert!(matches!(
            read_contained_to_string(&canonical_root, &path),
            Err(SourceReadError::Refused(_))
        ));
    }

    #[test]
    fn parent_directory_swapped_to_outside_symlink_is_refused() {
        let fixture = TempDir::new().expect("fixture");
        let root = fixture.path().join("root");
        let dir = root.join("src");
        fs::create_dir_all(&dir).expect("source directory");
        let path = dir.join("lib.rs");
        fs::write(&path, "pub fn inside() {}\n").expect("inside source");
        let outside = fixture.path().join("outside");
        fs::create_dir(&outside).expect("outside directory");
        fs::write(outside.join("lib.rs"), "pub fn outside() {}\n").expect("outside source");

        let canonical_root = root.canonicalize().expect("canonical root");
        assert!(path
            .canonicalize()
            .expect("checked path")
            .starts_with(&canonical_root));
        fs::rename(&dir, root.join("saved_src")).expect("move checked directory");
        symlink(&outside, &dir).expect("swap parent to outside symlink");

        assert!(matches!(
            read_contained_to_string(&canonical_root, &path),
            Err(SourceReadError::Refused(_))
        ));
    }

    #[test]
    fn bounded_reader_accepts_two_mib_and_refuses_one_byte_more() {
        let fixture = TempDir::new().expect("fixture");
        let root = fixture.path().canonicalize().expect("canonical root");
        let path = root.join("app.py");
        let declared_limit = 2 * 1024 * 1024;
        fs::write(&path, vec![b'x'; declared_limit]).expect("at-limit source");
        assert_eq!(
            read_contained_to_string(&root, &path)
                .expect("at-limit source is admitted")
                .len(),
            declared_limit
        );
        fs::File::create(&path)
            .expect("source")
            .set_len(declared_limit as u64 + 1)
            .expect("one byte over");
        assert!(matches!(
            read_contained_to_string(&root, &path),
            Err(SourceReadError::Refused(reason)) if reason.contains("2097152-byte")
        ));
    }

    #[test]
    fn regular_file_and_in_root_alias_are_read() {
        let fixture = TempDir::new().expect("fixture");
        let root = fixture.path();
        let path = root.join("lib.rs");
        fs::write(&path, "pub fn inside() {}\n").expect("inside source");
        let alias = root.join("alias.rs");
        symlink(&path, &alias).expect("in-root alias");
        let canonical_root = root.canonicalize().expect("canonical root");

        assert_eq!(
            read_contained_to_string(&canonical_root, &path).expect("regular file"),
            "pub fn inside() {}\n"
        );
        assert_eq!(
            read_contained_to_string(&canonical_root, &alias).expect("in-root alias"),
            "pub fn inside() {}\n"
        );
    }

    fn failure(reason: &str) -> io::Error {
        io::Error::new(io::ErrorKind::PermissionDenied, reason)
    }

    fn refusal_text(error: ContainedOpenError) -> String {
        let source_path = Path::new("/root/lib.rs");
        match contained_open_refusal(source_path, error) {
            SourceReadError::Refused(text) => text,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    fn open_refusal_text(canonical_root: &Path, path: &Path) -> String {
        match open_contained_file(canonical_root, path) {
            Err(SourceReadError::Refused(text)) => text,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn contained_open_refusals_keep_their_original_texts() {
        assert_eq!(
            refusal_text(ContainedOpenError::Open(failure("open failed"))),
            "cannot open candidate source /root/lib.rs: open failed"
        );
        assert_eq!(
            refusal_text(ContainedOpenError::Metadata(failure("stat failed"))),
            "cannot verify opened source type /root/lib.rs: stat failed"
        );
        assert_eq!(
            refusal_text(ContainedOpenError::NotRegular),
            "opened source is not a regular file: /root/lib.rs"
        );
        assert_eq!(
            refusal_text(ContainedOpenError::Resolve(failure("resolve failed"))),
            "cannot verify opened source /root/lib.rs: resolve failed"
        );
        let opened = PathBuf::from("/out/lib.rs");
        assert_eq!(
            refusal_text(ContainedOpenError::Escapes { opened }),
            "opened source escapes the canonical ingest root: /root/lib.rs -> /out/lib.rs"
        );
    }

    #[test]
    fn real_refusals_name_the_candidate_path_with_the_original_texts() {
        let fixture = TempDir::new().expect("fixture");
        let root = fixture.path().join("root");
        fs::create_dir(&root).expect("root");
        let canonical_root = root.canonicalize().expect("canonical root");
        let outside = fixture.path().join("outside.rs");
        fs::write(&outside, "pub fn outside() {}\n").expect("outside source");
        let escape = canonical_root.join("escape.rs");
        symlink(&outside, &escape).expect("outside symlink");
        let directory_path = canonical_root.join("src");
        fs::create_dir(&directory_path).expect("source directory");
        let missing = canonical_root.join("missing.rs");
        let missing_error = fs::File::open(&missing).expect_err("missing source");
        let opened = outside.canonicalize().expect("canonical outside");

        assert_eq!(
            open_refusal_text(&canonical_root, &missing),
            format!(
                "cannot open candidate source {}: {missing_error}",
                missing.display()
            )
        );
        assert_eq!(
            open_refusal_text(&canonical_root, &directory_path),
            format!(
                "opened source is not a regular file: {}",
                directory_path.display()
            )
        );
        assert_eq!(
            open_refusal_text(&canonical_root, &escape),
            format!(
                "opened source escapes the canonical ingest root: {} -> {}",
                escape.display(),
                opened.display()
            )
        );
    }
}
