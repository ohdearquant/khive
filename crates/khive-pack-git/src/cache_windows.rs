//! Windows command-path pins for an already-owned git cache slot (#2149).
//!
//! Git accepts a pathname, not a directory handle. Keep every directory in
//! that pathname open without delete sharing until the last child exits.
//! This prevents a validated slot or `.git` child from being renamed away
//! and replaced by a symlink; it does not make repository contents immutable.

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Storage::FileSystem::{
    FileIdInfo, GetFileInformationByHandleEx, GetFinalPathNameByHandleW,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_ID_INFO, FILE_NAME_NORMALIZED, FILE_SHARE_READ, FILE_SHARE_WRITE, VOLUME_NAME_DOS,
};

pub(super) struct PinnedSlot {
    git_dir: PathBuf,
    // Includes the initially resolved slot, every canonical ancestor, `.git`,
    // and the ownership marker. Never release before a command's wait ends.
    _pins: Vec<File>,
}

impl PinnedSlot {
    pub(super) fn open(repo: &Path) -> io::Result<Self> {
        // Open the final slot component itself, not a reparse-point target.
        // Existing aliases ABOVE the slot may resolve here; commands below
        // use only the resolved path, so those aliases are never re-used.
        let initial = open_pin(repo, true)?;
        let expected_identity = identity(&initial)?;
        let resolved = final_path(&initial)?;
        if !resolved.is_absolute() {
            return Err(invalid("slot handle did not resolve to an absolute path"));
        }

        let mut pins = vec![initial];
        // A final-path string alone is another TOCTOU. Pin root-to-leaf so
        // each next pathname resolves through parents already held open.
        // Verify the last handle still denotes the initial slot: an ancestor
        // may have moved between final_path() and opening this chain.
        for ancestor in resolved.ancestors().collect::<Vec<_>>().into_iter().rev() {
            pins.push(open_pin(ancestor, true)?);
        }
        let pinned_slot = pins
            .last()
            .expect("absolute path has a directory component");
        if identity(pinned_slot)? != expected_identity {
            return Err(invalid(
                "slot changed while its command path was being pinned",
            ));
        }

        let resolved_git_dir = resolved.join(".git");
        let git_pin = open_pin(&resolved_git_dir, true)?;
        let (git_dir, command_pin) = pin_git_command_path(&resolved_git_dir, &git_pin)?;
        pins.push(git_pin);
        pins.push(command_pin);
        pins.push(open_pin(&resolved.join(super::MARKER_FILE), false)?);
        Ok(Self {
            git_dir,
            _pins: pins,
        })
    }

    pub(super) fn git_dir(&self) -> &Path {
        &self.git_dir
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn pin_git_command_path(resolved: &Path, pinned: &File) -> io::Result<(PathBuf, File)> {
    let command_path = git_command_path(resolved)?;
    // The original ancestor chain is still pinned. Check the spelling Git
    // will receive against the held .git object and retain this handle too.
    // The lexical checks below matter even though this open succeeds: Rust
    // may internally add a verbatim prefix, whereas Git parses the ordinary
    // path. Names requiring verbatim semantics must never reach either path.
    let command_pin = open_pin(&command_path, true)?;
    if identity(&command_pin)? != identity(pinned)? {
        return Err(invalid(
            "Git command path does not name the pinned directory",
        ));
    }
    Ok((command_path, command_pin))
}

/// Convert only the handle-derived DOS/UNC spelling, never the caller's
/// original pathname. Git for Windows does not accept the `\\?\` spelling
/// returned by GetFinalPathNameByHandleW as --git-dir (#2149).
fn git_command_path(resolved: &Path) -> io::Result<PathBuf> {
    let verbatim = resolved
        .to_str()
        .and_then(|path| path.strip_prefix(r"\\?\"))
        .ok_or_else(|| invalid("Git command path needs a Unicode DOS or UNC handle path"))?;
    let (command, components) = if let Some(unc) = verbatim.strip_prefix(r"UNC\") {
        // Require a server, share and directory; neither a device namespace
        // nor a share-relative path can substitute for this absolute path.
        if unc.split('\\').count() < 3 {
            return Err(invalid("Git command path needs an absolute UNC directory"));
        }
        (format!(r"\\{unc}"), unc)
    } else {
        let bytes = verbatim.as_bytes();
        if bytes.len() < 4
            || !bytes[0].is_ascii_alphabetic()
            || bytes[1] != b':'
            || bytes[2] != b'\\'
        {
            return Err(invalid("Git command path needs an absolute DOS directory"));
        }
        (verbatim.to_owned(), &verbatim[3..])
    };
    // Removing a verbatim prefix enables ordinary Windows path parsing.
    // Refuse anything whose meaning could change instead of trimming,
    // normalizing, replacing characters, or finding an alternate spelling.
    for component in components.split('\\') {
        if !ordinary_component(component) {
            return Err(invalid("Git command path requires verbatim name semantics"));
        }
    }
    Ok(PathBuf::from(command))
}

fn ordinary_component(component: &str) -> bool {
    if component.is_empty()
        || component.ends_with([' ', '.'])
        || component.chars().any(|ch| {
            ch <= '\u{1f}' || matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
        })
    {
        return false;
    }
    // Device names remain reserved with an extension and regardless of
    // ASCII case. Include spaces before the extension and superscript port
    // digits; never turn a verbatim file component into a device reference.
    let stem = component
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end_matches(' ')
        .to_ascii_uppercase();
    if matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$" | "CLOCK$"
    ) {
        return false;
    }
    !stem
        .strip_prefix("COM")
        .or_else(|| stem.strip_prefix("LPT"))
        .is_some_and(|suffix| {
            matches!(
                suffix,
                "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
}

fn open_pin(path: &Path, directory: bool) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        // Windows delete access includes rename. An already-open DELETE
        // handle also makes this open fail, rather than weakening the pin.
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let metadata = file.metadata()?;
    // Check all reparse tags, including junctions, not only symlink_dir.
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || (directory && !metadata.is_dir())
        || (!directory && !metadata.is_file())
    {
        return Err(invalid(
            "cache ownership component has the wrong kind or is a reparse point",
        ));
    }
    Ok(file)
}

fn identity(file: &File) -> io::Result<(u64, [u8; 16])> {
    let mut info = FILE_ID_INFO::default();
    // SAFETY: file owns a live handle and info is a correctly sized writable
    // FILE_ID_INFO buffer for FileIdInfo. No handle ownership is transferred.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            (&raw mut info).cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((info.VolumeSerialNumber, info.FileId.Identifier))
}

fn final_path(file: &File) -> io::Result<PathBuf> {
    let mut buffer = vec![0u16; 260];
    loop {
        // SAFETY: file remains live; buffer exposes the stated writable
        // UTF-16 capacity for this call. No returned handle needs closing.
        let length = unsafe {
            GetFinalPathNameByHandleW(
                file.as_raw_handle(),
                buffer.as_mut_ptr(),
                buffer.len() as u32,
                FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
            )
        };
        if length == 0 {
            return Err(io::Error::last_os_error());
        }
        let length = length as usize;
        if length < buffer.len() {
            buffer.truncate(length);
            return Ok(PathBuf::from(OsString::from_wide(&buffer)));
        }
        buffer.resize(length.saturating_add(1), 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue2149_windows_command_path_converts_drive_and_unc_without_loss() {
        for (resolved, command) in [
            (r"\\?\C:\cache\slot\.git", r"C:\cache\slot\.git"),
            (
                r"\\?\d:\cache with spaces\资料😀\.git",
                r"d:\cache with spaces\资料😀\.git",
            ),
            (
                r"\\?\UNC\server\share\cache\slot\.git",
                r"\\server\share\cache\slot\.git",
            ),
            (
                r"\\?\UNC\server.example\share name\资料\.git",
                r"\\server.example\share name\资料\.git",
            ),
            (
                r"\\?\C:\COM10\ordinary..name\.git",
                r"C:\COM10\ordinary..name\.git",
            ),
        ] {
            assert_eq!(
                git_command_path(Path::new(resolved)).unwrap(),
                Path::new(command)
            );
        }
    }

    #[test]
    fn issue2149_windows_command_path_refuses_verbatim_only_and_ambiguous_names() {
        for resolved in [
            r"C:\cache\slot\.git",
            r"\\.\C:\cache\slot\.git",
            r"\\?\Volume{00000000-0000-0000-0000-000000000000}\slot\.git",
            r"\\?\GLOBALROOT\Device\HarddiskVolume1\slot\.git",
            r"\\?\C:relative\.git",
            r"\\?\UNC\server\share",
            r"\\?\UNC\\share\slot\.git",
            r"\\?\C:\cache\.\slot\.git",
            r"\\?\C:\cache\..\slot\.git",
            r"\\?\C:\cache\slot.\.git",
            r"\\?\C:\cache\slot \.git",
            r"\\?\C:\cache\\slot\.git",
            r"\\?\C:\cache\slot\.git\",
            r"\\?\C:\cache\slot:stream\.git",
            r"\\?\C:\cache\slot/child\.git",
            r"\\?\C:\cache\slot?\.git",
            r"\\?\C:\cache\slot*\.git",
            r"\\?\C:\cache\slot|\.git",
            r"\\?\C:\cache\slot<\.git",
            r"\\?\C:\cache\slot>\.git",
            "\\\\?\\C:\\cache\\slot\"\\.git",
            "\\\\?\\C:\\cache\\slot\0\\.git",
            "\\\\?\\C:\\cache\\slot\u{1f}\\.git",
        ] {
            assert!(
                git_command_path(Path::new(resolved)).is_err(),
                "{resolved:?}"
            );
        }
        for name in [
            "con",
            "NUL.txt",
            "NUL .txt",
            "COM1",
            "lpt9.log",
            "COM¹",
            "LPT².txt",
            "COM³",
            "CONIN$",
            "CONOUT$",
        ] {
            for prefix in [r"\\?\C:\cache", r"\\?\UNC\server\share"] {
                let resolved = format!(r"{prefix}\{name}\.git");
                assert!(
                    git_command_path(Path::new(&resolved)).is_err(),
                    "{resolved:?}"
                );
            }
        }
        let mut ill_formed: Vec<u16> = r"\\?\C:\cache\".encode_utf16().collect();
        ill_formed.push(0xd800);
        ill_formed.extend(r"\.git".encode_utf16());
        assert!(git_command_path(Path::new(&OsString::from_wide(&ill_formed))).is_err());
    }

    #[test]
    fn issue2149_windows_command_path_requires_the_pinned_directory_identity() {
        let dir = tempfile::tempdir().unwrap();
        let owned = dir.path().join("owned.git");
        let foreign = dir.path().join("foreign.git");
        std::fs::create_dir(&owned).unwrap();
        std::fs::create_dir(&foreign).unwrap();
        let owned_pin = open_pin(&owned, true).unwrap();
        let foreign_pin = open_pin(&foreign, true).unwrap();
        let (command, command_pin) =
            pin_git_command_path(&final_path(&owned_pin).unwrap(), &owned_pin).unwrap();
        assert!(!command.to_str().unwrap().starts_with(r"\\?\"));
        assert_eq!(
            identity(&command_pin).unwrap(),
            identity(&owned_pin).unwrap()
        );
        assert!(
            pin_git_command_path(&final_path(&foreign_pin).unwrap(), &owned_pin).is_err(),
            "an existing ordinary directory is insufficient without the pinned identity"
        );
    }
}
