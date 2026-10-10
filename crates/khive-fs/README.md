# khive-fs

Filesystem primitives shared across khive crates. The crate depends on no other khive crate.

## `fd_relative` (Unix only)

Helpers that resolve one path component against an open directory descriptor instead of walking a
pathname from the filesystem root:

- `stat_fd`, `stat_at`: `fstat` and `fstatat` that does not follow a final symlink.
- `open_at`, `open_dir_at`: read-only `openat` that refuses a final symlink.
- `open_file_at`: write-only or read-write opens through a borrowed directory descriptor, with
  explicit create/no-create/exclusive policy, non-blocking flag and umask-filtered creation mode.
  It always refuses final symlinks and sets close-on-exec; existing contents are not truncated.
- `unlink_at`: `unlinkat` of a file or symlink entry without following it; directories are
  refused and the entry's target is never touched.
- `rename_at`: `renameat` between two independently held directory descriptors, with the
  kernel's ordinary replacement and error semantics.
- `list_names`: the sorted entry names of a directory, without `.` and `..`, with a read error
  reported as an error rather than a short listing.
- `errno_location`, `clear_errno`, `current_errno`: the thread `errno` cell on every supported Unix
  platform.

A name must be a single component. An empty name and a name containing `/` are refused with
`InvalidInput` before any system call, as is a name with a NUL byte, so a name cannot be resolved
as a path that ignores the directory descriptor. `.` and `..` are single components and are
accepted: `list_names` reopens a directory through `.`, and a directory walk steps up through `..`.
What `..` means is the caller's policy for these existing read/directory helpers. `open_file_at`
additionally refuses both dot components before opening a writable entry.

Otherwise the helpers return the raw `std::io::Error` of the failing call; callers add their own
context.

## `atomic_publish` (Unix only)

`stage_atomic_at` exclusively creates a staging file under a held directory, invokes a writer
callback, then syncs and closes that file. `publish_atomic_at` additionally renames the staged
file over its destination and syncs the directory. Use the staged operation when a multi-file
checkpoint has separate metadata and segment publication boundaries.

Passing a `StaleTmp` retains mode 0644 filtered by umask; `AtomicPublishOptions`
selects a creation mode explicitly. Both operations use no-follow and close-on-exec.
`StaleTmp::Refuse` replaces a stale regular file but refuses nonregular entries, including
symlinks. `StaleTmp::Unlink` removes a stale file or symlink without following it; directories
refuse. `StaleTmp::RefuseExisting` instead refuses every existing entry through exclusive
creation, without inspecting or unlinking an incumbent. This additive enum variant must be
handled by exhaustive downstream matches. Existing policies retain their behavior.

Names accept `AsRef<OsStr>` without lossy UTF-8 conversion, including `open_file_at`.
Both names are checked before publication has any filesystem effect: empty, slash-containing,
NUL-containing and dot names refuse, as do equal staging and destination names. Backslash is an
ordinary Unix name; callers can impose stricter naming policy.

The `_detailed` variants retain the failing phase and original `io::Error`; the convenience
functions return that original error directly, preserving native errno. No error automatically
cleans up a partial staging file. A directory-sync failure happens after rename and does not mean
the old destination was restored. Callers own directory validation, writer serialization, and any
multi-file commit protocol. Holding the directory pins its inode even if its pathname changes.

## `directory_walk` (Unix only)

`walk_to_directory(path, policy, budget)` opens every directory along a path with `O_DIRECTORY` and
`O_NOFOLLOW`, one component at a time, and returns every pinned handle with the final directory
last. A component that is a symlink is followed only when the caller's `LinkPolicy` accepts it:

- `before_follow` sees the link's name, whether it is the last component, its metadata and the
  pinned parent directory, and may refuse the link with its own error.
- `after_read` runs once the target has been read and does nothing unless a policy overrides it.
- `budget` bounds the links the walk follows; a link met after it is spent ends the walk with a
  `BudgetExhausted` error.

`AncestorLinkPolicy::new(AncestorWalkEndpoint)` is the standard policy for mirror, segment and
WAL-pin directory walks, as proposed in [ADR-198](../../docs/adr/ADR-198-shared-fs-ancestor-link-policy.md).
It trusts root/effective-user links and parents, permits group/other write only with sticky
protection, refuses a macOS parent ACL that grants any right or cannot be inspected, and rechecks
the link's device/inode/mode/uid after reading its target. Consumers pass the common
`ANCESTOR_LINK_BUDGET` of eight.

`FinalTarget` refuses a last-component link; `TargetParent` lets the last component be an ancestor
when the caller separately opens its final target without following links. The policy's errors
carry a named `AncestorLinkRefusal`, with available ownership/mode evidence and an underlying
witness error where applicable. It does not replace the caller's final-object validation.

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
