use std::{fs::File, path::Path};

use memmap2::MmapOptions;

/// A temporary, read-only view of a checkpoint segment. Empty files remain an
/// empty slice so checksum and format validation keep their existing order.
#[cfg(feature = "mmap")]
pub(super) struct MappedCheckpointSegment {
    mmap: Option<memmap2::Mmap>,
}

#[cfg(all(test, feature = "mmap"))]
impl Drop for MappedCheckpointSegment {
    fn drop(&mut self) {
        if self.mmap.is_some() {
            super::checkpoint_allocation_tests::record_mapping_closed();
        }
    }
}

#[cfg(feature = "mmap")]
impl std::ops::Deref for MappedCheckpointSegment {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        self.mmap.as_deref().unwrap_or(&[])
    }
}

#[cfg(feature = "mmap")]
pub(super) fn map_checkpoint_segment(path: &Path) -> std::io::Result<MappedCheckpointSegment> {
    let file = File::open(path)?;
    if file.metadata()?.len() == 0 {
        return Ok(MappedCheckpointSegment { mmap: None });
    }
    // SAFETY: reads use publication locks or the existing read-only identity
    // checks; the sequence guard holds the writer's exclusive lock. Like vectors
    // and codes, a live segment must not be mutated or truncated outside that
    // protocol. Temporary mappings close before a rebuild replaces the files.
    let mmap = unsafe { MmapOptions::new().map(&file)? };
    #[cfg(test)]
    super::checkpoint_allocation_tests::record_mapping_opened();
    Ok(MappedCheckpointSegment { mmap: Some(mmap) })
}
