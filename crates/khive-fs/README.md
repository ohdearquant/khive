# khive-fs

Filesystem primitives shared across khive crates. The crate depends on no other khive crate.

## `fd_relative` (Unix only)

Helpers that resolve one path component against an open directory descriptor instead of walking a
pathname from the filesystem root:

- `stat_fd`, `stat_at`: `fstat` and `fstatat` that does not follow a final symlink.
- `open_at`, `open_dir_at`: `openat` that refuses a final symlink.
- `list_names`: the sorted entry names of a directory, without `.` and `..`, with a read error
  reported as an error rather than a short listing.
- `errno_location`, `clear_errno`, `current_errno`: the thread `errno` cell on every supported Unix
  platform.

The helpers return the raw `std::io::Error` of the failing call; callers add their own context.

## `opened_file`

Helpers that judge the file that was actually opened instead of the pathname that was checked
before the open:

- `opened_file_path`: the path the kernel reports for an open file. Linux and Android read
  `/proc/self/fd`, macOS and iOS use `F_GETPATH`, Windows uses the final path name of the handle,
  and any other target reports `ErrorKind::Unsupported`.
- `open_regular_file_within`: opens a path read-only and returns the file only if it is a regular
  file whose resolved path is inside a canonical root. A refusal is a `ContainedOpenError`
  variant, and the variants that come from a failing call carry its raw `std::io::Error`.
