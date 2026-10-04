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

## `directory_walk` (Unix only)

`walk_to_directory(path, policy, budget)` opens every directory along a path with `O_DIRECTORY` and
`O_NOFOLLOW`, one component at a time, and returns every pinned handle with the final directory
last. A component that is a symlink is followed only when the caller's `LinkPolicy` accepts it:

- `before_follow` sees the link's name, whether it is the last component, its metadata and the
  pinned parent directory, and may refuse the link with its own error.
- `after_read` runs once the target has been read and does nothing unless a policy overrides it.
- `budget` bounds the links the walk follows; a link met after it is spent ends the walk with a
  `BudgetExhausted` error.
