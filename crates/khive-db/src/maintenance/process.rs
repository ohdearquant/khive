use super::record::{CoordinatorIdentity, RecordRefusal};

pub(super) fn same_incarnation(
    expected: &CoordinatorIdentity,
    observed: Result<CoordinatorIdentity, RecordRefusal>,
) -> Result<(), RecordRefusal> {
    if &observed? != expected {
        return Err(super::record::invalid("writer incarnation changed"));
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub(super) fn capture() -> Result<CoordinatorIdentity, RecordRefusal> {
    Err(RecordRefusal::UnsupportedAttribution)
}

#[cfg(target_os = "linux")]
pub(super) fn capture() -> Result<CoordinatorIdentity, RecordRefusal> {
    use super::record::{invalid, ImageIdentity};
    use std::os::unix::fs::MetadataExt;

    let before = parse_stat(&std::fs::read_to_string("/proc/self/stat")?)?;
    let boot_id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?
        .trim()
        .to_owned();
    let boot_uuid =
        uuid::Uuid::parse_str(&boot_id).map_err(|_| invalid("invalid proc boot identity"))?;
    if boot_uuid.hyphenated().to_string() != boot_id {
        return Err(invalid("noncanonical proc boot identity"));
    }
    let metadata = std::fs::metadata("/proc/self/exe")?;
    let image = if metadata.ino() != 0 {
        ImageIdentity::Inode {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    } else {
        let (before_epoch, duration) =
            match metadata.modified()?.duration_since(std::time::UNIX_EPOCH) {
                Ok(value) => (false, value),
                Err(error) => (true, error.duration()),
            };
        ImageIdentity::Modified {
            before_epoch,
            seconds: duration.as_secs(),
            nanoseconds: duration.subsec_nanos(),
        }
    };
    // SAFETY: geteuid has no arguments or memory preconditions.
    let uid = unsafe { libc::geteuid() };
    let after = parse_stat(&std::fs::read_to_string("/proc/self/stat")?)?;
    if before != after || before.0 != std::process::id() {
        return Err(invalid("unstable proc incarnation"));
    }
    Ok(CoordinatorIdentity {
        uid,
        pid: before.0,
        boot_id,
        start_ticks: before.1,
        image,
    })
}

#[cfg(any(target_os = "linux", test))]
pub(super) fn parse_stat(stat: &str) -> Result<(u32, u64), RecordRefusal> {
    use super::record::invalid;
    let start = stat.find('(').ok_or_else(|| invalid("missing proc comm"))?;
    let end = stat
        .rfind(')')
        .filter(|end| *end > start)
        .ok_or_else(|| invalid("invalid proc comm"))?;
    let pid: u32 = stat[..start]
        .trim()
        .parse()
        .map_err(|_| invalid("invalid proc pid"))?;
    // Fields after ')' begin at field 3; starttime is field 22 (index 19).
    let ticks: u64 = stat[end + 1..]
        .split_whitespace()
        .nth(19)
        .ok_or_else(|| invalid("missing proc starttime"))?
        .parse()
        .map_err(|_| invalid("invalid proc starttime"))?;
    if pid == 0 || ticks == 0 {
        return Err(invalid("zero proc incarnation"));
    }
    Ok((pid, ticks))
}
