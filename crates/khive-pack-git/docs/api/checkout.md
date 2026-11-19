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
