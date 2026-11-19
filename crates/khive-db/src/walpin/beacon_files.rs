#[cfg(unix)]
use super::unix_impl;
#[cfg(windows)]
use super::windows_impl;
use super::{io, Path, PathBuf, WalpinBeacon, WalpinHeartbeat};

/// Ensure `dir` exists and is trustworthy: a real directory (never a
/// symlink or reparse-point component), and accessible only to its owner:
/// Unix mode `0700`, or a protected owner-only DACL on Windows. Refuses —
/// never chmod/chown/re-ACLs — a non-compliant existing directory.
pub fn ensure_sidecar_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        unix_impl::SidecarDirHandle::open_or_create(dir)?;
        Ok(())
    }
    #[cfg(windows)]
    {
        windows_impl::ensure_sidecar_dir(dir)
    }
}

/// Write (or refresh) this process's heartbeat file. Exclusive-create a temp
/// file (`O_NOFOLLOW` on Unix), then atomically rename it over the target —
/// never an in-place open of a possibly attacker-placed path.
pub fn write_heartbeat(dir: &Path, heartbeat: &WalpinHeartbeat) -> io::Result<()> {
    let body =
        serde_json::to_vec(heartbeat).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let target = format!("{}.json", heartbeat.pid);
    let tmp = format!(".{}.json.tmp", heartbeat.pid);
    #[cfg(unix)]
    {
        let handle = unix_impl::SidecarDirHandle::open_or_create(dir)?;
        handle.write_atomic(&target, &tmp, &body)
    }
    #[cfg(windows)]
    {
        windows_impl::write_atomic(dir, &target, &tmp, &body)
    }
}

/// ADR-091 Amendment 3 Plank F1: a metadata-only mtime touch of this
/// process's already-written heartbeat — no data write, mirroring
/// [`touch_beacon`]'s mechanism and opened-directory-descriptor discipline
/// exactly. Must run on every sweep tick where the warn condition persists
/// and the heartbeat's content has not changed; a content change still
/// goes through [`write_heartbeat`]. Callers must not assume the target
/// exists — enumeration can delete a slow writer's heartbeat while its
/// span is still live — and must recreate via [`write_heartbeat`] on any
/// touch failure, never treat the record as gone for good.
pub fn touch_heartbeat(dir: &Path, pid: u32) -> io::Result<()> {
    let name = format!("{pid}.json");
    #[cfg(unix)]
    {
        let handle = unix_impl::SidecarDirHandle::open_or_create(dir)?;
        handle.touch_mtime(&name)
    }
    #[cfg(windows)]
    {
        windows_impl::touch_mtime(dir, &name)
    }
}

/// Remove this process's registration beacon, if present (fail-closed
/// escalation for a failing heartbeat write path — see the sidecar
/// `observe` logic in `khive-db`'s checkpoint module). Never follows a
/// symlink at the target path. A missing sidecar directory is a no-op — it
/// must NOT be created as a side effect of a removal.
pub fn remove_beacon(dir: &Path, pid: u32) -> io::Result<()> {
    let target = format!("{pid}.beacon");
    #[cfg(unix)]
    {
        match unix_impl::SidecarDirHandle::open_if_exists(dir)? {
            Some(handle) => handle.remove_checked(&target),
            None => Ok(()),
        }
    }
    #[cfg(windows)]
    {
        windows_impl::remove_checked(dir, &target)
    }
}

/// Remove this process's heartbeat file, if present. Never follows a
/// symlink at the target path. A missing sidecar directory is a no-op — it
/// must NOT be created as a side effect of a removal.
pub fn remove_heartbeat(dir: &Path, pid: u32) -> io::Result<()> {
    let target = format!("{pid}.json");
    #[cfg(unix)]
    {
        match unix_impl::SidecarDirHandle::open_if_exists(dir)? {
            Some(handle) => handle.remove_checked(&target),
            None => Ok(()),
        }
    }
    #[cfg(windows)]
    {
        windows_impl::remove_checked(dir, &target)
    }
}

/// Write this process's one-time registration beacon (ADR-091 Amendment 2
/// sidecar-health attribution). Written once at sidecar initialization; see
/// [`touch_beacon`] for the required per-tick freshness refresh — a beacon
/// that is never refreshed again classifies as stale, never
/// `registered-silent` (beacon refresh rule).
pub fn write_beacon(dir: &Path, beacon: &WalpinBeacon) -> io::Result<()> {
    let body =
        serde_json::to_vec(beacon).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let target = format!("{}.beacon", beacon.pid);
    let tmp = format!(".{}.beacon.tmp", beacon.pid);
    #[cfg(unix)]
    {
        let handle = unix_impl::SidecarDirHandle::open_or_create(dir)?;
        handle.write_atomic(&target, &tmp, &body)
    }
    #[cfg(windows)]
    {
        windows_impl::write_atomic(dir, &target, &tmp, &body)
    }
}

/// ADR-091 Amendment 2 beacon refresh rule: a metadata-only mtime touch of
/// this process's already-written beacon — no data write, preserving the
/// zero-steady-state-data-traffic property. Must run on every sweep tick
/// while the beacon exists: `registered-silent` classification requires the
/// refresh timestamp (not just the original write) to stay within the
/// freshness window.
pub fn touch_beacon(dir: &Path, pid: u32) -> io::Result<()> {
    let name = format!("{pid}.beacon");
    #[cfg(unix)]
    {
        let handle = unix_impl::SidecarDirHandle::open_or_create(dir)?;
        handle.touch_mtime(&name)
    }
    #[cfg(windows)]
    {
        windows_impl::touch_mtime(dir, &name)
    }
}

/// Path of `pid`'s one-time registration beacon under `dir`.
pub fn beacon_path(dir: &Path, pid: u32) -> PathBuf {
    dir.join(format!("{pid}.beacon"))
}
