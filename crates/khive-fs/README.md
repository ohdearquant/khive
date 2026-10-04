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

## `directory_walk` (Unix only)

`walk_to_directory(path, policy, budget)` opens every directory along a path with `O_DIRECTORY` and
`O_NOFOLLOW`, one component at a time, and returns every pinned handle with the final directory
last. A component that is a symlink is followed only when the caller's `LinkPolicy` accepts it:

- `before_follow` sees the link's name, whether it is the last component, its metadata and the
  pinned parent directory, and may refuse the link with its own error.
- `after_read` runs once the target has been read and does nothing unless a policy overrides it.
- `budget` bounds the links the walk follows; a link met after it is spent ends the walk with a
  `BudgetExhausted` error.

## `opened_file`

Helpers that judge the file that was actually opened instead of the pathname that was checked
before the open:

- `opened_file_path`: the path the kernel reports for an open file. Linux and Android read
  `/proc/self/fd`, Apple targets (macOS, iOS and the other Apple platforms) use `F_GETPATH`,
  Windows uses the final path name of the handle, and any other target reports
  `ErrorKind::Unsupported`.
- `open_regular_file_within`: opens a path read-only and returns the file only if it is a regular
  file whose resolved path is inside a canonical root. A refusal is a `ContainedOpenError`
  variant, and the variants that come from a failing call carry its raw `std::io::Error`.
