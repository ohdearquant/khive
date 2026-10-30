# Workspace model

A workspace is a durable graph container for records that belong to one human or agent working
context. It is an ordinary entity plus typed membership edges, not a private table or service.

Identity comes from the generic entity ID and name. `schema_version` makes the properties payload
evolvable, while `filesystem_path` remains a non-unique mutable hint: moving a checkout must not
change graph identity or invalidate the record.

Membership broadens the base entity-to-entity `contains` contract to five already-shipped note
kinds. Reusing the shared edge relation keeps graph traversal uniform and avoids introducing a
workspace-specific membership verb. The v0 pack therefore adds vocabulary and validation only.

## Workspace mirror pass

A pass visits directory entries in sorted order and returns counts together with diagnostics for
non-UTF-8 files, paths with no unique current document, refused writes, and entries skipped when
their type or identity changes. Diagnostic paths are masked and are not stable identifiers: two
different paths may produce the same masked value. A skipped file does not advance its cursor.
A previously mirrored file with a retained cursor and no live current document is not recreated.

On Unix, traversal uses directory handles pinned for that pass, with descriptor-relative no-follow
opens. This anchors operations to the opened directory capabilities; it does not prevent hard links,
mounts, or movement of an already-open directory, and it does not bind root identity across passes.
The root label is resolved after pinning and checked against the pinned device and inode before
traversal. If that directory is renamed and its old path aliases the same inode, the pass reads
the pinned directory's own contents and labels them by its resolved location at that check.
The root's final component must not be a symlink. Roots with trailing separators, `.` or `..`
components, or `/` itself are refused before reading. Non-regular handles are never read; opening
a swapped device can itself have an effect even though the open is nonblocking. On non-Unix
systems, opened files are checked against the resolved root; directory traversal remains
pathname-based and may race with directory replacement.
