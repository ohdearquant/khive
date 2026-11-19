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
#[path = "safe_source_tests.rs"]
mod tests;
