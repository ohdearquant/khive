# Server file transfers

`blob.import(path, media_type?)` streams a regular file on the machine running
the server into its installed BlobStore. It returns `{content_ref, size}` and,
when supplied, `media_type`. It never returns file bytes. The reference is the
BLAKE3 of the bytes accepted by the existing staged-upload manager. Files are
limited to 64 MiB; length changes during ingestion refuse the upload. The
manager's existing admission, incremental hash, cleanup and expiry rules apply.
An interrupted import remains tracked for cleanup. Backends that do not support
staged uploads refuse import; there is no whole-file buffering fallback. At
present FsBlobStore supports staging and S3BlobStore does not.

`path` is relative to `~/.khive/imports`, or is an absolute path beneath its
canonical root. `KHIVE_IMPORT_FROM_ROOT` overrides that root. Empty paths,
`..` components, files outside the root, symlinks anywhere within the supplied
path below the root, and non-regular files are refused. Absolute paths must
also be spelled beneath the canonical root. The opened object is checked again
for regular-file type, with no-follow opening on Unix.

`blob.export(content_ref, path)` verifies and hydrates an existing object, at
most 64 MiB, through the runtime's shared admission controller. It writes the
admitted buffer to a securely created temporary file in the destination's
directory, then renames that complete file atomically over the destination.
It returns `{path, size}`, never bytes. Failure before the rename leaves the
previous destination intact; an interrupted caller may not know whether the
file task completed. Its destination policy is shared with `request(save_to=...)`:
relative paths use `~/.khive/exports`, `KHIVE_SAVE_TO_ROOT` overrides the root,
`..` and paths whose resolved parent escapes that root are refused, and an
existing destination must be a regular file rather than a symlink or directory.

Both verbs require the canonical import and export roots to be disjoint at
each call. Equal roots and nesting in either direction refuse both operations.
Both operator and wire calls use confinement. Both verbs are classified as
write operations and refuse on a read-only runtime. `blob.get` retains its
existing base64 and frame limits.

These paths refer to the server filesystem. Multitenant deployments must not
expose `blob.import` or `blob.export`. There is no automatic hosted-surface
detector. Configure roots and their ancestor directories so only trusted
processes can change them: the path checks, like the existing save sink policy,
do not fence concurrent directory swaps by another local writer.

The optional import media type is receipt metadata. BlobStore stat provides
size, not a MIME catalog; passing a reference to comm therefore records a
nullable media type rather than recovering the import label.
