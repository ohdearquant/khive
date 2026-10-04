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

A name must be a single component. An empty name and a name containing `/` are refused with
`InvalidInput` before any system call, as is a name with a NUL byte, so a name cannot be resolved
as a path that ignores the directory descriptor. `.` and `..` are single components and are
accepted: `list_names` reopens a directory through `.`, and a directory walk steps up through `..`.
What `..` means is the caller's policy.

Otherwise the helpers return the raw `std::io::Error` of the failing call; callers add their own
context.
