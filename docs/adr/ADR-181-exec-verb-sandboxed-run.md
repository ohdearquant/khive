# ADR-181: Exec Verb: One Declared Command in a Sandbox over a Materialized Tree

- **Status**: Accepted (2026-09-09, implemented by the exec pack)
- **Date**: 2026-09-08
- **Extends**: [ADR-111](ADR-111-blob-store.md) (content-addressed objects; this record adds a tree
  manifest over them), [ADR-180](ADR-180-tool-pack.md) (the policy vocabulary every run is checked
  against)
- **Relates to**: [ADR-108](ADR-108-git-write-surface.md) (git writes take the same tree),
  [ADR-085](ADR-085-code-pack.md) (the code pack ingests and runs nothing; this record is where running
  lives), [ADR-018](ADR-018-authorization-gate.md) (the Gate still runs on every dispatch)

## Context

An agent that develops on khive today still builds and tests through its own shell. Nothing in the
verb set executes a command, so the one step of the loop that must touch a toolchain happens outside
every policy, receipt and sandbox khive has. The blob store holds objects but has no notion of a tree,
so a set of files cannot be named, moved or compared as one thing. The intended posture is an
allow-list: the agent's whole tool set is khive verbs, a command that is not a registered tool does
not exist for it, and the shell exists only inside one verb, over a tree khive materialized, with no
network, no home directory and no credentials.

## Decision

A pack `exec`, requiring `blob` and `tool`. Verb names below are the proposal; the contract tests
written for the loop driver finalize them.

**Tree manifest.** A tree is a blob whose content is `{"schema": "khive-tree/v1", "entries": [{"path",
"ref", "mode"}]}`: `path` is relative, normalized, contains no `..` or absolute component and names no
symlink; `ref` is a blob reference; `mode` is `644` or `755`. `exec.tree(entries)` validates and stores
one and returns its reference; `exec.tree_get(tree)` returns the entries; `exec.tree_diff(base, head)`
lists `added`, `modified` and `deleted` paths with both references. The same manifest is what the git
verbs read and write (ADR-182).

**Run.** `exec.run(tree, tool, args, env, timeout_s, actor)`:

1. Policy first. `tool` names a registry object of kind `tool` with `source` `exec:<absolute binary
   path>`, registered by the operator; an unregistered name is refused before anything touches the
   disk. `tool.check(actor, tool)` must answer `allow`; `ask` and `deny` are returned as the refusal
   with the decision, so the agent's next call is `tool.request`, never a retry.
2. Materialize. A fresh run directory under the daemon's exec root receives every entry from the blob
   store with its mode; nothing else is present.
3. Sandbox. The command runs under `sandbox-exec` with a seatbelt profile compiled from a fixed
   template: default deny; read access to the run directory and to the toolchain roots listed in the
   `[exec] read_roots` config; write access to the run directory and one run-scoped temporary
   directory; no network; the environment is the allow-listed keys from `[exec] env` plus `HOME` set
   to the run directory; no credential of the daemon or the operator reaches the process. The binary
   is the registered absolute path, never a `PATH` lookup. The profile text is stored with the receipt
   as a digest.
4. Bound. `timeout_s` (default and maximum from config) kills the process group; stdout and stderr are
   captured to blobs up to a configured size each and truncated with a marker beyond it.
5. Capture. After exit the run directory is walked; every entry whose content changed, every new file
   and every deleted entry is recorded, changed content stored as blobs, and a result tree manifest
   written; the run directory is removed unless `keep` is set and permitted by config.
6. Receipt. One row in the pack table `exec_runs`: id, namespace, actor, tool, argv, `tree_in`,
   `tree_out`, exit code, `timed_out`, stdout and stderr references, started and finished times,
   duration, the policy decision and its source id, the profile digest. The return value is the
   receipt plus the change list. `exec.receipt(id)` and `exec.runs(actor, tool, limit)` read them.

The run never reads or writes a repository; the git verbs (ADR-182) move trees in and out of git.

## Acceptance

1. An unregistered tool name is refused before materialization: no run directory is created.
2. A registered tool with a `deny` policy is refused with `decision: deny` and the policy id; with
   `ask`, refused with `ask`; after `tool.grant`, the same call runs.
3. Materialization reproduces the tree: every entry's content and mode match; a manifest with `..`,
   an absolute path or a symlink entry is refused by `exec.tree`.
4. No network: a registered tool that opens a socket fails inside the run; the same binary outside the
   sandbox succeeds (control).
5. No writes outside the tree: a run that writes to the operator's home leaves no file there and
   reports a non-zero exit; a write inside the tree appears in the change list.
6. Timeout: a run exceeding `timeout_s` ends with `timed_out: true` and no process survives it.
7. Capture classifies added, modified and deleted paths; every reference returned resolves through
   `blob.get` to the content the run left.
8. The receipt's `tree_in`, `tree_out`, output references and profile digest match the stored
   objects; `exec.runs(actor)` lists it.

## Known rough edges

The sandbox is macOS seatbelt only; a Linux profile is a later slice. Toolchain read roots are operator
configuration and a missing root reads as a tool failure, not a policy refusal. Every run materializes
from scratch; build caches across runs are a later slice. Output caps and run retention are config,
not policy.

## Amendment 1 (2026-09-08): run parameters, capture, refusal receipts, sandbox identity

Adopted on the first whole-file read against the loop driver's contract page. Each item is a
contract the driver's tests bind to; the record above stands where not restated.

1. **`cwd`.** `exec.run` takes `cwd`, a path relative to the tree, default `.`. An absolute path, a
   `..` component or a symlink escape is refused before materialization.
2. **Environment.** The caller supplies the environment values; the `[exec] env` config allow-lists
   the keys that may pass; the server sets `HOME` to the run directory; nothing is inherited from the
   host. The receipt records `env_keys`.
3. **`declared_write_paths`** (optional). When given, a change outside the declared set makes the run
   `success: false`, names the offending paths in `undeclared_changes`, and drops their bytes: they
   are neither stored nor part of `tree_out`.
4. **Capture over the cap keeps the tail.** For each stream the receipt carries `produced_bytes`,
   `retained_bytes` and `capture: complete | incomplete`; a test runner's closing summary survives.
5. **Every refusal writes a receipt.** An unregistered tool, a `deny` or `ask` decision, an invalid
   tree and a `cwd` escape each write an `exec_runs` row with `decision` and `reason`, `tree_out`
   null, and no run directory is created.
6. **Session identity.** `exec.run` takes an optional `session_id`, echoed in the receipt with a
   per-session `seq`; `exec.runs` filters on it. The driver's command identity is `session:seq`.
7. **Sandbox object.** The receipt carries `sandbox: {profile_digest, tool_binary_digest,
   read_roots_digest}` (the resolved read roots and the registered binary hashed at run time), not a
   bare profile digest.
8. **Version control never runs here.** A registered tool whose binary resolves, after
   canonicalization, to `git` or `gh`, or to any path in the `[exec] never` config set, is refused;
   repository operations go through ADR-182 only.
9. **Driver mapping.** An `argv` from the driver binds `argv[0]` to a registered tool label and passes
   `argv[1..]` as `args`; this lives in the driver's test file and changes nothing here.

Acceptance arms added: 9 `cwd` escape refused with no run directory; 10 an env key outside the
allow-list is absent inside the run and a caller value for an allowed key is present; 11 a write
outside `declared_write_paths` yields `success: false` and the bytes are not retrievable; 12 output
over the cap retains the tail and the receipt's counts match the bytes produced; 13 each refusal
class has a receipt row; 14 two runs with one `session_id` carry `seq` 1 and 2; 15 the sandbox object
changes when the read roots change (control); 16 a tool registered at the `git` binary is refused.

## Amendment 2 (2026-09-08): limits per platform, file-size semantics, profile identity

Adopted on the first native run of the loop driver's exec contract on macOS.

1. **Limits the platform cannot enforce per run are refused at load.** `[exec] limits` accepts
   `cpu_seconds`, `address_space`, `file_size` and `nproc`. On macOS the address-space limit is not
   settable and the process limit counts every process of the user, so a configuration naming either
   is refused at config load with `[exec] limits.<name>: unsupported_on_platform`; the daemon does not
   start and the reason is in the startup log. The receipt's `limits` carries `requested` (the config)
   and `enforced` (the child's own report of the limits it received).
2. **File-size semantics.** A write that crosses the file-size limit is truncated to the limit with no
   signal; the signal fires on a write attempted at the limit. A run over the limit therefore ends by
   signal 25 or, for a runtime that ignores that signal, by a failed write reported in its own stream.
   The receipt's `exit_signal` and captured streams are the evidence, never the exit status alone.
3. **Profile identity.** The seatbelt profile allows reads of the root directory, the system read
   roots, the configured read roots and the run directory, and writes only under the run directory.
   `exec.identity` reports the read roots, their digest, the profile template digest and the digest
   algorithm, so a client can check its expectation of the sandbox before it runs anything.

Acceptance arms added: 17 a configuration naming `address_space` or `nproc` refuses at startup with
the named limit; 18 a run over `cpu_seconds` ends by signal 24 and one over `file_size` ends by
signal 25 or a failed write, with the receipt's `enforced` limits equal to the configuration; 19
`exec.identity` and the receipt's `sandbox` agree on the read-roots digest, and the digest changes
when a read root is added (control).

## Amendment 3 (2026-09-08): declared write paths, sequence allocation, version control at the kernel boundary

Adopted after the review that followed the first exec implementation. Each item makes exact a rule
the implementation already followed loosely; nothing above is withdrawn.

1. **Declared write paths are a prefix set at path boundaries.** Each entry of `declared_write_paths`
   is a tree-relative path under the same validation as a tree entry (no leading `/`, no empty, `.`
   or `..` segment). An entry covers exactly itself and every path below it separated by `/`: `src`
   covers `src` and `src/main.rs`, never `src.bak`. A change is any path whose bytes or mode differ
   from `tree_in`, any path added, and any path missing at the end of the run, and every change is
   tested against the set. An undeclared change goes to the receipt's `undeclared_changes`, its bytes
   are not stored, `tree_out` carries the `tree_in` entry for that path (a deleted file reappears
   with its old content, an added file is absent), and `success` is false even when the exit status
   is zero. An absent `declared_write_paths` declares every path.
2. **Sequence numbers are allocated by the insert.** The per-session `seq` is computed inside the
   statement that inserts the receipt row (`MAX(seq) + 1` over the namespace and session), and a
   unique index over `(namespace, session_id, seq)` for session rows turns any duplicate into a
   constraint failure instead of an overlap. The column is authoritative: `exec.run`, `exec.receipt`
   and `exec.runs` all report it, a refusal consumes a number like a run, and a run without a session
   carries `null`.
3. **Version control is denied at the kernel boundary.** Beside the registered-binary refusal of
   Amendment 1 item 8, the seatbelt profile denies `process-exec` for any executable whose file name
   is `git` or `gh` or starts with `git-`, and for every canonical path in the `[exec] never` set. A
   run whose registered binary is allowed but which reaches git from inside (a shell, a build script,
   a package manager hook) gets an operation-not-permitted failure from the kernel, visible in the
   child's exit status and captured stderr, and the receipt records it like any other failed run. The
   profile template digest changes with this rule; `exec.identity` reports the `never` set beside it.

Acceptance arms added: 20 with `declared_write_paths = ["a", "b"]` a write to `a/x`, a new `a/y`
and a deletion of `b` are listed in `changed`, a write to `a.bak` is listed in `undeclared_changes`
with its input entry kept in `tree_out`, and `success` is false with exit status zero; 21 two runs
started concurrently in one session receive `seq` 1 and 2, `exec.runs` for the session lists both,
and `exec.receipt` reports the same number as the run reply; 22 a shell run that invokes
`git --version` and one that invokes a `never` path both end with a non-zero status and the kernel's
refusal in stderr, while the same shell invoking `/bin/echo` exits zero (control).

## Amendment 4 (2026-09-09): `exec.tree_put`, a batch edit that mints one tree or none

A tree is an immutable manifest blob, so every edit mints a new manifest. Callers today build the
whole entry list themselves and call `exec.tree`, which means an agent editing a checkout has to
read the old tree, splice its own changes into the entry array, and re-declare every path it did
not touch. `exec.tree_put(tree, edits)` does that splice, and it takes a list rather than a single
path because a caller that edits several files per step would otherwise mint one throwaway tree per
file, each of which nothing ever reads.

1. **The shape.** `exec.tree_put(tree, edits)` returns `{tree, base, entries, changed}`, where
   `changed` is the `exec.tree_diff` of the base against the result. Each edit is an object with a
   `path` and exactly one of `ref` (an existing blob reference), `content` (bytes to store), or
   `delete: true`. An edit that names none of the three, or more than one, is refused and the
   refusal says how many it named. `mode` is optional on a put and forbidden on a delete: an edit
   that omits it keeps the mode the path already had, and a new path takes 644. The modes a
   manifest stores are the decimal 644 and 755, never octal literals.

2. **One new tree or none.** The atomicity is a property of the result, not of the loop that
   produces it: a call yields exactly one new tree reference or none, and a refusal on any edit
   leaves the blob store with no new object from the call, including objects for the edits that
   were fine. That is why `content` bytes are hashed rather than written while the call is being
   validated. `digest_hex` is the same BLAKE3 the blob store keys on, so a content edit's reference
   is known before its byte is stored, and only a call that will succeed writes anything.
   The guarantee is scoped to refusals: it covers every refusal the verb raises, all of which are
   raised during validation, and it does not cover a blob store that fails partway through
   publication. Once validation passes, the content blobs are published one at a time and the
   manifest last, so a backend failure during publication leaves the objects already published and
   mints no tree. Those objects are referenced by no manifest, which is the same state an
   interrupted `blob.put` leaves and is what the store's own reclamation is for; a caller reading
   the refusal still knows no new tree exists, which is the property the atomicity claim is about.

3. **The candidate manifest goes through the pack's own validator.** After the edits are applied,
   the complete entry list is validated by `parse_entries`, the same function `exec.tree` uses. This
   is not tidiness. `parse_entries` enforces that a file cannot also be a directory prefix of
   another entry, and a verb that builds `TreeEntry` values directly and stores them would happily
   mint a manifest containing both `a` and `a/b` that `exec.tree_get` would then refuse to load.
   Deleting `a` and adding `a/b` in one call is legitimate and passes, because the candidate the
   validator sees no longer holds `a` as a file.

4. **Refusals that a quiet success would hide.** Duplicate paths in one list are refused and the
   refusal names both indices, rather than last-one-wins: these lists are produced by generated code
   and by models, both of which produce duplicates, and last-one-wins makes the caller's second
   intent vanish where nothing downstream can observe that it happened. A delete of a path the tree
   does not hold is refused, because a silent no-op is how a caller comes to believe it removed
   something. An empty `edits` list is refused rather than returning the input tree, because an
   empty edit is almost always a caller bug and echoing the input makes a no-op look like work.

5. **Degrade safety.** `exec.tree_put` is a Declaration and a writer, so it is not on the admission
   degrade-safe list beside `exec.tree_get` and `exec.tree_diff`.

Acceptance arms added: 23 a call that rewrites one path by content, replaces another by ref, and
adds two new paths returns a new tree whose entries carry the expected modes (kept, given, and the
644 default), leaves the base tree readable at its pre-edit content, and stores the new bytes so
they read back; 24 a delete removes only the named path, a delete of an absent path is refused
naming it, and a delete carrying a mode is refused; 25 duplicate paths are refused with both
indices and the path in the message; 26 an empty edits list is refused; 27 an edit naming zero or
two of ref/content/delete is refused counting them, an out-of-range mode is refused, an octal
literal mode is refused as the 420 it is, and an escaping path is refused; 28 adding `a/b` where
`a` is a file is refused, while deleting `a` and adding `a/b` in one call succeeds; 29 a list whose
last edit fails normalization leaves the blob store object count unchanged, with the same list
minus that edit as a positive control that moves the count; 30 a `ref` naming no stored object is
refused with the count unchanged, so the good edit's blob was not written first.

## Amendment 5 (2026-09-10): symlink entries

1. **Manifest modes.** `khive-tree/v1` accepts the decimal modes `644`, `755` and
   `120000`. The third mode represents a symlink: its blob contains the literal
   target path bytes, exactly as a Git `120000` blob does, with no added newline,
   encoding conversion or normalization. This is an additive entry mode, not a
   new manifest shape, so the schema string remains `khive-tree/v1` and existing
   manifests remain valid. Entry paths retain their existing validation; no entry
   may be below a file or symlink entry, and duplicate paths are refused.

2. **Tree editing and comparison.** `exec.tree` and `exec.tree_put` accept
   `120000`; `exec.tree_get` returns it. For a symlink put, `content` supplies the
   target's UTF-8 bytes or `ref` names a blob holding arbitrary target bytes.
   Omitted mode preserves the existing entry mode, and deletion is unchanged.
   Changing the target or switching between a file and a symlink is `modified`.
   `git.diff(input_kind="trees")` maps this mode to Git's `120000` blob entry,
   retaining native symlink add, retarget, remove and file-conversion patches.

3. **Materialization and containment.** Materialization creates a real symlink
   with its literal target, whether relative, absolute, dangling or outside the
   tree. The target is not constrained to the tree root. A fresh run directory
   receives all directories and exclusively created files before any symlinks,
   so filesystem name aliases cannot redirect materialization writes. The seatbelt profile
   remains the read/write boundary: writing through a symlink does not grant
   write access to its resolved target outside the run directory. A denied write
   is visible through the command's exit status and captured stderr.
   `declared_write_paths` still names tree-relative paths, not resolved targets;
   it filters captured changes and grants no additional filesystem access.

4. **Capture.** The capture walk records symlinks with mode `120000` and reads
   their target bytes without following them. Directory symlinks are entries,
   never traversal roots. Created, retargeted, removed and mode-flipped links
   appear in `changed` under the same declaration rules as files. Sockets,
   FIFOs and devices remain skipped.

Acceptance arms added: 31 valid symlink entries pass while unsupported modes and
entries beneath symlinks fail; 32 tree edits preserve target bytes and classify
all link transitions; 33 materialization preserves literal relative, escaping,
absolute and non-UTF-8 targets; 34 an escaping-link write leaves its outside
target unchanged and reports a sandbox denial, while an inside-tree write
succeeds; 35 capture records links without descending through directory links;
36 tracked file, directory and dangling symlinks round-trip through the manifest
to a Git tree with an empty `git diff-tree`, and a link-to-file conversion emits
the native `120000` to `100644` change.

## Amendment 6 (2026-09-10): observed limiting resource on receipts

Every newly written run receipt, including a refusal receipt, carries the explicit
`limiting_resource` key. Its closed vocabulary is `cpu_seconds | address_space |
file_size | nproc | null`; `null` is serialized rather than omitted. `exec.run`,
`exec.receipt` and `exec.runs` return the same stored value. Historical receipts are
not rewritten or assigned an inferred cause.

The wait site records `cpu_seconds` only for a delivered `SIGXCPU` when that run
configured `cpu_seconds`, and `file_size` only for a delivered `SIGXFSZ` when it
configured `file_size`. Normal exits, nonzero exit codes, unrelated signals
(including generic `SIGKILL`), wait errors and the run-timeout path leave `null`.
The field is not derived afterwards from the exit code or merely from a requested
limit. Existing signal termination keeps `exit_code: null` and the delivered
signal in `exit_signal`; enforcement and timeout behavior are unchanged.

The observation is limited to the directly waited child's signal. A descendant
whose parent converts a signal into an ordinary exit code supplies no such
observation. Likewise, `ENOMEM` from an address-space limit and `EAGAIN` from a
process limit occur inside the child, so this wrapper records `null` for
`address_space` and `nproc` even on Linux. An ignored `SIGXFSZ` followed by an
`EFBIG` write error is not inferred as `file_size`. The macOS startup refusal for
unsupported `address_space` and `nproc` remains unchanged and produces no receipt.

Acceptance arms added: 37 a spinning child under a one-second CPU limit records
`cpu_seconds`; 38 a child writing past a file-size limit records `file_size`; 39
exit zero, nonzero exit, `SIGTERM`, generic `SIGKILL` and timeout each retain a
present JSON null; 40 the same spin without a CPU limit reaches its wall timeout
and retains null; 41 each resource signal without its matching configured limit
retains null. The arms read back the durable receipt and its listing as well as
the run reply. Replacing observation with a nonzero-exit heuristic must fail the
unrelated-signal arm; omitting null must fail the exit-zero arm.
