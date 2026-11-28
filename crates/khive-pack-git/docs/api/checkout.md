# Checkout object reads

`git.checkout(repo, ref)` reads repository objects into an ADR-181 tree manifest,
without changing the working tree or index. See
[ADR-182 Amendment 2, item 8](../../../../docs/adr/ADR-182-git-dev-loop-verbs.md).
Ordinary blobs share one `cat-file --batch` child and one filter configuration
snapshot. Missing or non-blob responses use the native `cat-file blob` fallback.
Every blob is bounded at 64 MiB, independently of the aggregate tree size, and
batch headers are bounded at 128 bytes. The manifest is admitted before any blob
read or write. The caller completes each `BlobStore.put` before acknowledging its
frame, and that acknowledgement permits the worker to request the next object.

`git.checkout(repo, ref, paths)` reads only the entries at or beneath 1 to 64
named repository paths, each a file or a directory, into a scoped tree
(`khive-tree/scoped-v1`) that records them in sorted order. See
[ADR-182 Amendment 24](../../../../docs/adr/ADR-182-git-dev-loop-verbs.md). The
paths are checked before git runs: a leading or trailing slash, an empty, `.` or
`..` component or a NUL refuses `invalid_path`, and a path inside another
refuses `overlapping_paths`. The listing is
`git --literal-pathspecs ls-tree -r -z --full-tree <commit> -- <paths>`, so a
path is matched as its literal text. A path that names nothing at the commit
refuses `path_not_found`, naming it, before any blob is read. The manifest
bounds and blob reuse above apply unchanged; a scoped checkout is never answered
from an earlier checkout's tree. `git.commit` refuses a scoped tree.

The declared size limit intentionally takes priority as soon as a valid batch
header advertises more than 64 MiB. An oversized loose blob whose compressed
object file is truncated after its intact header therefore returns `output_limit`
(`git output exceeds the whole-blob limit`), even if reading its body would later
make native Git exit 128. The former per-object reader waited for native completion
and returned `git_failed` with that exit status for this input. Both refuse and
store no bytes for the failed entry. This is an intentional error-code priority
change, not a guarantee that every native failure retains its former code.
Within the size limit, a truncated frame that reaches EOF retains the observed
native exit-status refusal.

Cancellation kills and reaps the child. Malformed output seen before EOF also
terminates it immediately. Once EOF has been observed, the reader waits for native
completion to establish the exit-status refusal; an operator-configured program
that closes stdout and keeps running can wait until cancellation. This existing
custom-program behavior is not a bounded-completion guarantee.
