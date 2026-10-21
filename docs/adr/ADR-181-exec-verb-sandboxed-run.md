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
4. Bound. `timeout_s` (default and maximum from config) kills the initial process group; stdout and stderr are
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
4. No network: a registered tool that attempts an outbound connection or binds and listens on a
   network port fails inside the run; both operations succeed outside the sandbox (controls).
5. No writes outside the tree: a run that writes to the operator's home leaves no file there and
   reports a non-zero exit; a write inside the tree appears in the change list.
6. Timeout: a run exceeding `timeout_s` ends with `timed_out: true` and no process survives it.
   This remains a target, not a claim that the current implementation meets it; see Amendment 11.
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

## Amendment 7 (2026-09-11): the sandbox object is open, and the descendant case gets an arm

Amendment 1 item 7 named the receipt's `sandbox` object as exactly three digests. It is not
exhaustive and it was never meant to be read as a closed set. A run prepared today carries five
keys, and the two that are not digests were added after that sentence was written:

```json
"sandbox": {
  "profile_digest": "...",
  "tool_binary_digest": "...",
  "read_roots_digest": "...",
  "tool_source": "exec:/absolute/path/to/the/registered/binary",
  "tool_registry_id": "..."
}
```

`tool_source` and `tool_registry_id` report the registration the run actually resolved, which is
not the same fact as the binary's hash: two registry rows can name one binary, and a caller
auditing what was approved needs the row. Neither is a check against a caller's expectation. The
rule this amendment states is the general one: **`sandbox` is open to additive fields, and a
consumer that treats its key set as closed will break on the next addition.** Read the keys you
need by name.

`sandbox` is `null`, not a partial object, when the run is refused before the sandbox is prepared.
A reader distinguishing "no sandbox" from "sandbox without a tool source" is reading a state that
does not occur.

The second half of this amendment is an acceptance arm, not a change. Amendment 6 already says the
`limiting_resource` observation is limited to the directly waited child, and that a descendant
whose parent converts a signal into an ordinary exit code supplies no observation. That sentence is
correct and it is now the only thing holding the property: arms 37 through 41 all bound the DIRECT
child, so nothing fails if a later change starts inferring the cause from the exit code in exactly
the case the prose excludes. A harness exercising the surface from outside met this and had to read
the prose to tell a documented limit from a defect.

Acceptance arm added: 42 a run whose direct child spawns a descendant that exceeds a configured
resource limit, where the parent reaps the descendant's signal and exits with an ordinary nonzero
code, records `limiting_resource: null` while the enforcement itself still holds (the descendant
is killed and its output truncated at the limit). The control is arm 37's shape, the same limit
exceeded by the direct child, which records `cpu_seconds`. The pair is what separates "observation
is limited to the waited child" from "observation is broken", and an implementation that derives
the resource from a nonzero exit code passes 37 and must fail 42.

## Amendment 8 (2026-09-12): effective output cap on receipts

Every newly written run receipt, including refusals and failed or truncated runs, carries
`effective_max_output_bytes`: the resolved unsigned byte cap from `[exec] max_output_bytes`
for that run. The cap applies independently to stdout and stderr retention, not to their sum
or to the total output a child may produce. Zero retains no stream bytes. The value is
captured before preflight, even when the run is refused before any output is produced.

`exec.run`, `exec.receipt` and `exec.runs` expose the same stored value. Historical receipts
remain readable with the field absent; readers must treat that absence as an unknown cap,
not infer it from the current configuration or from the retained byte count. No historical
receipt is rewritten.

The existing `stdout_produced_bytes`, `stderr_produced_bytes`, `stdout_retained_bytes`,
`stderr_retained_bytes` and capture statuses already distinguish short output from
truncation. The new field records the configuration that governed retention, including when
both streams fit below it; it does not change capture behavior or add a run parameter.

Acceptance: small configured caps are preserved on complete and refusal receipts; truncated
stdout and stderr retain fewer bytes than produced, matching the cap and preserving their
tails; zero retains no bytes. New receipts round-trip through receipt lookup and listing,
and decoding an older receipt leaves the unknown cap absent.

## Amendment 9 (2026-09-25): what the version-control and `never` denial guarantees

Status: Accepted (2026-09-25). Refs #3303, #3304. This amendment
changes no enforcement. It states the guarantee of Amendment 3 item 3 exactly, and says which part of
the profile bounds what a run can do to a repository.

### Context

Amendment 3 item 3 is headed "Version control is denied at the kernel boundary" and says that a run
"which reaches git from inside (a shell, a build script, a package manager hook) gets an
operation-not-permitted failure from the kernel". The mechanism it names is accurate:
`render_profile` in `crates/khive-pack-exec/src/sandbox.rs` emits `(allow process-exec)` followed by a
`(deny process-exec ...)` whose filters are a regex on the final path component (`git`, `gh`, or a
`git-` prefix) and a `literal` for each canonical path in the `[exec] never` set. The kernel enforces
that rule on every exec in the sandbox. What the rule matches is the path an executable is launched
from, not what the executable is. The same profile allows writes to the run directory and maps it
executable, which build-and-test runs need because they execute what they build there. So the heading
and the outcome sentence claim more than a path rule provides: a program launched from a path the rule
does not name is not refused, whatever it does. The `[exec]` configuration documentation
(`crates/khive-runtime/src/engine_config.rs`, `ExecSectionConfig`) describes `never` as "which binaries
never run", which overstates it the same way.

Separately, the same profile allows file metadata reads (`file-read-metadata`) without a path filter,
while step 3 limits reads to the run directory, the configured read roots and the fixed system paths.
Step 3 is the rule; the profile is broader than it. That is tracked as a code defect in #3304, and
this amendment does not change step 3.

### Decision

1. **The guarantee, stated exactly.** The kernel refuses `process-exec` of any file whose path ends in
   a component named `git` or `gh`, or a component starting with `git-`, and of any file at a canonical
   path listed in `[exec] never`. The rule identifies an executable by its path only. It guards against
   a run reaching version control or a listed binary by its usual name or location, for example from a
   build script or a package-manager hook; it is not a prohibition on any capability, and a program
   reached through another path is not matched by it.
2. **What bounds a run's effect on repositories.** A run's effects are bounded by the rest of the
   profile, not by this rule: writes only under the run directory (and `/dev/null`), no network
   operation, and no credential of the daemon or the operator in the environment. Whatever a program
   inside a run does, it acts only on the run's materialized tree and reaches no remote. Trees reach a
   repository only through the git verbs of ADR-182.
3. **Wording.** Amendment 3 item 3 is read as headed "version-control and `never` executables are
   refused by path", and its outcome sentence as applying to git reached under a path the rule names.
   The `[exec] never` configuration documentation and the `exec.identity` description of the `never`
   set say that the set matches canonical paths.

### Alternatives considered

- _Make the claim true by removing execution from the run directory._ Rejected as the default: the
  purpose of this verb is building and testing, and a build executes what it produces in the run
  directory (test binaries, generated scripts). It would also not turn the rule into a capability
  boundary, because an interpreter under a read root can perform the same operations without
  executing a file of a matching name. A per-tool opt-in that removes run-directory execution for tools
  that do not need it is compatible with this amendment and can be proposed separately, with its own
  acceptance arm.
- _Leave the text as accepted._ Rejected: the text is a security claim, and a reader relying on it
  would treat the `never` set as a hard prohibition it cannot be.

### Consequences

- No profile change, so the profile template digest does not change.
- Operators who configure `never` read it as a guard against accidental invocation of those paths, and
  rely on write confinement, the absence of network access and the absence of credentials for
  containment.
- Acceptance: arms 4 (no network), 5 (no writes outside the tree) and 22 (a shell invoking `git` by
  name and a `never` path is refused, `/bin/echo` is the control) are the arms this guarantee rests on
  and stay required. The `[exec]` configuration documentation and the `exec.identity` description are
  updated to the wording in item 3 in the same change that accepts this amendment.

## Amendment 10 (2026-09-28): bounded binary digest and one run deadline

The registered binary's canonical path and forbidden-binary identity are checked before the tool
policy decision, as Amendment 1 item 8 requires. Reading its content for the receipt's
`tool_binary_digest` begins only after `tool.check` allows the selected registry row. An `ask` or
`deny` decision never hashes that binary.

The digest reads at most 256 MiB plus one byte to detect an over-limit file, using the same 256 MiB
budget as run input materialization. `[exec] binary_digest_timeout_s` defaults to 10 seconds and
accepts an integer from 1 through 60; it is independent of `timeout_s`, which governs the spawned
run. This deadline guards against stalled mounts and is not a performance budget. Exceeding the
byte budget refuses with `binary_digest_byte_limit`; exceeding the digest deadline refuses with
`binary_digest_time_limit`. A failed or incomplete digest never appears in a receipt as a valid
digest.

Digest refusals carry a typed wire code and structured evidence: `elapsed_ms`, `bytes_read`,
`byte_cap`, `time_cap_ms`, and `path_class` when the opened file type is known (`null` otherwise).
The durable refusal receipt keeps the same code and evidence under `refusal`. A read or worker
failure has its own `binary_digest_read` or `binary_digest_worker` code and may carry a cause.

The run wall deadline is established when spawning begins. Reading the child's resource-limit
report and waiting for the child spend that same `timeout_s` budget. If the deadline expires
during report collection, the report is uncertified and the existing timeout branch kills and
reaps the directly waited child after killing its initial process group with `timed_out: true`.

Acceptance: a denied registered tool causes no binary digest attempt; an over-budget binary gets
the named byte-limit refusal; a digest reader stalled past `binary_digest_timeout_s` gets
`binary_digest_time_limit` with `elapsed_ms >= time_cap_ms` and no digest in any receipt; a delayed
report consumes the wall budget so a child cannot receive a fresh full timeout after report
collection.

## Amendment 11 (2026-09-29): timeout residual for detached descendants

Status: Accepted (2026-09-30) as a statement of current behavior; the containment target in
acceptance 6 is **not met** for a descendant that leaves the initial process group (for example,
by calling `setsid`). The follow-up design issue is #3631. This amendment does not change the
Seatbelt profile and leaves acceptance 6's lifetime target intact. The receipt-visible changes it
makes, output collection setting `timed_out`, a capture path cap and a `tree_capture` status, are
stated below.

Acceptance 4 now names the operations the Seatbelt profile actually refuses: outbound connect and
bind/listen on a network port, each with an unsandboxed control. Its earlier "opens a socket"
wording was too broad: `socket()` can return an unconnected descriptor inside the sandbox, while
the attempted connection or bind is refused. This clarification replaces acceptance 4's prior
predicate; it does not claim that opening an unconnected descriptor is denied.

On a run deadline, the wrapper signals the initial process group and waits for the directly
spawned child. A descendant that has moved into another process group can remain alive after
`exec.run` returns. `timed_out: true` in a receipt or exit event reports that the run deadline was
reached; it does not certify that every descendant has exited. The `exited` event concerns the
directly waited child. The run's output collection has a separate bounded close grace, so output
references and capture status also do not prove descendant termination.

The detached descendant retains the Seatbelt profile inherited at launch. Its continued lifetime
does not grant writes outside the allowed roots or network reach. A macOS regression arm must let
a `setsid` descendant live past the timed-out receipt, then observe a refused write outside the
run root, a refused outbound connection, and a refused bind/listen. The same operations must
succeed from an unsandboxed process; launching the survivor unsandboxed is the must-fail mutation
control. This tests confinement after the wrapper returns; it does not satisfy acceptance 6's
lifetime target.

Until #3631 resolves the design, consumers must treat `timed_out: true` as a deadline outcome and
must not infer whole-process-tree termination from it.

`timed_out: true` is also set when output collection reaches the run deadline after the directly
waited child has exited, so a receipt can carry `exit_code: 0` beside `timed_out: true`. The flag
reports the run deadline, not how the child ended.

Capture lists the run directory from the descriptor opened before launch and reopens each
directory from that root one component at a time. A path longer than 1024 bytes relative to the
run directory is a capture error, which bounds the reopen work for each directory; the earlier
path-based walk stopped near the platform path limit.

After the walk, capture asks the kernel for the pinned run directory's current path (`F_GETPATH` on
macOS) and checks with `lstat` that the path still names that directory. A run directory the tool
removed, or removed and recreated at the same path, lists as empty through the pinned descriptor,
and on macOS its `st_nlink` does not reach zero, so without this check the receipt would read as a
run that wrote nothing. When the path is gone or names another file, the receipt's `tree_capture`
is `degraded` and `tree_capture_detail` starts with `root_missing` and names the detector. When the
path cannot be queried or read, for example because the tool moved the directory to a path longer
than the platform limit, `tree_capture` is also `degraded` and the detail starts with
`root_unverified`. In both cases `success` is false, no `tree_out` or `changed` entries are
published, and `exit_code` keeps the tool's own status. A renamed run directory is still the pinned
directory and is captured through it, including when the tool then creates a new directory at the
old path: the receipt describes the renamed tree, and files written to the new directory are not
captured. Otherwise `tree_capture` is
`complete`, `failed` for a capture error, or `none` when the run did not reach capture. A removal
after the check is not detected, and a platform with no descriptor-to-path query runs no such check.

## Amendment 12 (2026-10-03): descendant identity, cleanup evidence and capture

**Status: Accepted (2026-10-03; see Ratification below).** This is a design proposal for #3631 and the remaining process-lifetime
question in #3291. It has not been ratified and authorizes no implementation by itself. The accepted
text above, including Amendment 11, remains binding. Acceptance 6 is still an unmet target on the
shipped backend.

### Boundary examined

The backend at this proposal's source revision is macOS Seatbelt through `/usr/bin/sandbox-exec`.
Startup refuses a host without that launcher; there is no shipped Linux confinement backend. The
[profile](../../crates/khive-pack-exec/src/sandbox.rs) allows `process-exec` and `process-fork`. The
[launcher](../../crates/khive-pack-exec/src/handlers.rs) creates an initial session, clears the host
environment, waits for the direct child and signals the initial group. A descendant can change its
group or session. The existing detached-survivor fixture deliberately observes it after the receipt;
that fixture tests retained file and network restrictions, not whole-tree termination.

This proposal distinguishes four requirements: admit an authentic run member, acquire a capability
bound to that exact process, have permission and a supported interface to act on it, and establish
that every member exited. None implies the others. A copied marker, matching UID, PID or PID plus a
previously read birth time is insufficient. Reading a birth time or `p_uniqueid` again and then
calling `kill(pid, signal)` still has a check-to-signal race. Reopening a process handle from an
already stale PID has the same admission problem. A numeric process-group ID retained after its
original leader has been reaped is also not a lifetime capability.

### Mechanisms and counterexamples

Costs below are source-derived work descriptions, not measurements. Let P be the number of
processes examined and S the number of sweep rounds. An algorithm doing one linear census per round
examines O(P × S) census records; ancestry traversal, identity admission, stop acknowledgement and
settlement add work. No algorithm or overall cost bound is selected here. All observation and
cleanup work needs finite process, byte, round and time limits; a limit or inaccessible process
produces incomplete evidence, never certification of an empty run.

| Mechanism                                                                            | What it can establish                                                                                                                                              | Escape or missing proof                                                                                                                                                                                                                                                                                     | Cost and disposition                                                                                                                                                                                                                        |
| ------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Parent-chain census, stop, then repeated enumeration                                 | Discover currently connected descendants; after confirmed stops, another round can find children created before those stops.                                       | A double fork whose intermediate parent exited before discovery has lost that current parent chain. Sending SIGSTOP is not acknowledgement that every thread is quiescent. PID and parent-PID reuse can misidentify membership or the signal target.                                                        | Repeated census records plus ancestry, identity and stop acknowledgements. An unchanged snapshot is a fixed point of the observation, not proof that no orphan exists. Do not use it as certification.                                      |
| Per-run argument or environment marker                                               | A descendant retaining the marker may be found after reparenting or `setsid`.                                                                                      | A program can exec with another environment, erase its user-stack marker or copy it to another same-UID process. Restricted targets can hide environment values. A marker authenticates neither membership nor identity.                                                                                    | Bounded metadata reads for P processes per round. Treat matches as diagnostic leads only; never signal on the marker alone.                                                                                                                 |
| Dedicated supervisor                                                                 | Keeps its own children waitable and can retain identities admitted before execution. Linux child-subreaper semantics additionally adopt eligible orphans.          | A normal macOS parent does not acquire every double-fork orphan. Polling has the same discovery gap. Subreaping alone supplies no atomic termination boundary.                                                                                                                                              | Another long-lived process, IPC, retained member state and crash recovery. A complete, non-bypassable admission mechanism remains required. No macOS subreaper equivalent was established by the examined interfaces.                       |
| Seatbelt denial of `SYS_setsid` and `SYS_setpgid`, followed by initial-group cleanup | A platform probe reported refusal of those two direct Unix syscalls. It establishes a concrete candidate rule rather than an inferred profile operation.           | The same probe reported escape through `POSIX_SPAWN_SETSID` and `POSIX_SPAWN_SETPGROUP`. Denying two syscall entries does not prevent their internal spawn paths. Current group cleanup has no certified empty-group observation.                                                                           | Policy checks and compatibility work. Session/group-changing calls receive EPERM on the probed host; supported tool coverage and other creation routes remain unverified. Do not ship this rule plus `killpg` as containment certification. |
| Retained Mach task capability or private audit-token signalling                      | Potentially act on an exact admitted target without a later bare-PID signal.                                                                                       | Acquisition and authenticated lineage need proof. Access is permission-dependent; the examined audit-token interfaces are private, and identity versions can change across exec. Signalling is not a general identity-bound wait API.                                                                       | Per-member acquisition, lifecycle handling, OS/SDK compatibility and permission gates. A feasibility route, not a selected production dependency.                                                                                           |
| Darwin coalition                                                                     | Kernel task membership, inherited through relevant task creation, and empty-state notification are stronger ingredients than a parent-chain snapshot.              | Run-scoped creation and explicit admission have privilege or private-entitlement gates. A terminate request does not kill members and does not immediately freeze all activation. A supported per-run creation, admission, termination and empty-notification chain is not established for this deployment. | Retained coalition state, privileged controller/admission and notification lifecycle. Actual daemon coalition membership must be checked; UID alone neither grants nor disproves access. Not a selected backend.                            |
| Per-run guest using `Virtualization.framework`                                       | If all run execution stays in the guest, completed destructive VM stop supplies a boundary for guest execution independent of its process tree.                    | Guest shutdown requests are not exit evidence. Host helpers, writable host shares, output buffers and storage completion need separate boundaries. Entitlement, host support and configuration admission must succeed before launch.                                                                        | Guest OS/image, boot, CPU/memory, output transport and tool compatibility costs. No cost is measured. A named alternative requiring a separate backend design, not a feature of the current launcher.                                       |
| Separate Linux cgroup v2 backend                                                     | With membership established before untrusted execution and escape prevented, kernel group termination and an empty-group observation can support a stronger claim. | Moving an already running process into a group leaves a launch race; writable migration paths can invalidate membership. Kernel support, delegation and sandbox policy need explicit admission.                                                                                                             | A group per run, a privileged or delegated controller and lifecycle monitoring. Future backend work, unavailable through the shipped Seatbelt launcher.                                                                                     |

### Seatbelt probe and source boundary

A platform probe reported results on macOS 27.0, build 26A428, arm64, using this profile:

```scheme
(version 1)
(allow default)
(deny syscall-unix (syscall-number SYS_setsid SYS_setpgid))
```

Forked-child `setsid()` and `setpgid(0, 0)` succeeded without that profile and returned EPERM under
it. In contrast, `posix_spawn` with `POSIX_SPAWN_SETSID` or `POSIX_SPAWN_SETPGROUP` created a new
session or group with and without the profile. These are reported observations for that host,
not a supported-SDK guarantee, a complete route census or a tool compatibility benchmark.

The pinned XNU [Unix syscall dispatch](https://github.com/apple-oss-distributions/xnu/blob/f6217f891ac0bb64f3d375211650a4c1ff8ca1ea/bsd/dev/arm/systemcalls.c#L160-L167)
calls `mac_proc_check_syscall_unix` when the syscall filter rejects an entry. This is the relevant
hook; generic scheduling or signal hooks do not establish that rule's behavior. The
[spawn attribute path](https://github.com/apple-oss-distributions/xnu/blob/f6217f891ac0bb64f3d375211650a4c1ff8ca1ea/bsd/kern/kern_exec.c#L4485-L4502)
calls `setpgid` and `setsid_internal` internally. That source path explains why filtering the two
standalone syscall entries cannot by itself close the measured spawn escape. XNU also has
[a fork policy check](https://github.com/apple-oss-distributions/xnu/blob/f6217f891ac0bb64f3d375211650a4c1ff8ca1ea/bsd/kern/kern_fork.c#L286-L295),
but a hook does not prove a compatible complete policy for fork, vfork, spawn and every escape
attribute. Those additional route and deployment checks remain outstanding.

Apple's pinned [Libc `daemon()` implementation](https://github.com/apple-oss-distributions/Libc/blob/71bbe350ab79eef58113991d817ccc6165061a64/gen/FreeBSD/daemon.c#L93-L110)
forks and then calls `setsid`, returning failure if that call fails. This identifies an API
compatibility cost of the candidate rule by source; it is not a measured `daemon()` probe or a
claim about any particular tool. A supported tool inventory, helper creation patterns and exact
OS/SDK policy availability require their own evidence. An unknown profile operation was also
reported to be rejected by the parser; that does not authorize inventing another rule name.

### Identity, membership and settlement evidence

Apple's pinned [libproc header](https://github.com/apple-oss-distributions/xnu/blob/f6217f891ac0bb64f3d375211650a4c1ff8ca1ea/libsyscall/wrappers/libproc/libproc.h#L41-L44)
marks those interfaces private. The [audit-token signal path](https://github.com/apple-oss-distributions/xnu/blob/f6217f891ac0bb64f3d375211650a4c1ff8ca1ea/bsd/kern/proc_info.c#L3564-L3624)
checks identity and policy, reacquires a validated process reference and retains it through the
signal. Its [identity lookup](https://github.com/apple-oss-distributions/xnu/blob/f6217f891ac0bb64f3d375211650a4c1ff8ca1ea/bsd/kern/kern_proc.c#L485-L519)
validates process generation and unique identity. This refutes the blanket claim that Darwin has
no identity-bound signalling mechanism. It does not establish complete run membership or supported
availability on the deployment target. The [exec path changes the identity version](https://github.com/apple-oss-distributions/xnu/blob/f6217f891ac0bb64f3d375211650a4c1ff8ca1ea/bsd/kern/kern_exec.c#L7096-L7105);
refreshing a stale token requires renewed identity admission.

[Wait functions operate on child processes](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/wait4.2.html).
Waiting on an unreaped owned child does not supply a wait capability for arbitrary discovered
nonchildren. A complete sweep design must separately prove identity-bound exit observation,
permission, lifecycle races and supportedness for those nonchildren. It cannot append `waitpid`
to a private signal call and claim settlement. The required combination may be unavailable to the
deployed daemon; its availability must not be assumed from the existence of either API alone.

[Process-args sysctl](https://github.com/apple-oss-distributions/xnu/blob/f6217f891ac0bb64f3d375211650a4c1ff8ca1ea/bsd/kern/kern_sysctl.c#L1328-L1406)
can omit environment values for restricted processes even with a matching UID. Ordinary
[orphan reparenting](https://github.com/apple-oss-distributions/xnu/blob/f6217f891ac0bb64f3d375211650a4c1ff8ca1ea/bsd/kern/kern_exit.c#L2471-L2474)
goes to `initproc` in the examined source. Darwin's [event header](https://github.com/apple-oss-distributions/xnu/blob/f6217f891ac0bb64f3d375211650a4c1ff8ca1ea/bsd/sys/event.h#L258-L262)
does not deliver the child PID with `NOTE_FORK`, and its recursive flags are
[unsupported since 10.5](https://github.com/apple-oss-distributions/xnu/blob/f6217f891ac0bb64f3d375211650a4c1ff8ca1ea/bsd/sys/event.h#L362-L369).
They do not supply the missing recursive tracker.

Darwin's [coalition syscall gate](https://github.com/apple-oss-distributions/xnu/blob/f6217f891ac0bb64f3d375211650a4c1ff8ca1ea/bsd/kern/sys_coalition.c#L220-L249)
requires privileged coalition membership for create, terminate and reap. The
[membership check](https://github.com/apple-oss-distributions/xnu/blob/f6217f891ac0bb64f3d375211650a4c1ff8ca1ea/osfmk/kern/coalition.c#L1607-L1620)
checks a coalition's privileged flag, not a blanket root-UID rule. Initial coalitions are
[privileged](https://github.com/apple-oss-distributions/xnu/blob/f6217f891ac0bb64f3d375211650a4c1ff8ca1ea/osfmk/kern/coalition.c#L2337-L2348),
so a daemon's actual membership cannot be inferred from its non-root UID. Explicit
[spawn selection](https://github.com/apple-oss-distributions/xnu/blob/f6217f891ac0bb64f3d375211650a4c1ff8ca1ea/bsd/kern/kern_exec.c#L4070-L4085)
requires privileged membership or the private coalition-spawn entitlement. Ordinary
[inheritance](https://github.com/apple-oss-distributions/xnu/blob/f6217f891ac0bb64f3d375211650a4c1ff8ca1ea/osfmk/kern/task.c#L1929-L1967)
is distinct from admission into a newly selected run coalition.

The [terminate operation](https://github.com/apple-oss-distributions/xnu/blob/f6217f891ac0bb64f3d375211650a4c1ff8ca1ea/osfmk/kern/coalition.c#L2179-L2233)
requests notification and reaches terminated state when its active count is zero; it does not kill
member tasks. The [activation check](https://github.com/apple-oss-distributions/xnu/blob/f6217f891ac0bb64f3d375211650a4c1ff8ca1ea/osfmk/kern/coalition.c#L1488-L1519)
rejects terminated/reaped state, not a terminate request alone. A termination request must not be
described as an immediate freeze of all admission. Supported notification access, complete
membership and member termination must be proved as a chain. Enumerating coalition member PIDs
then signalling those numbers would still reintroduce the check-to-signal race. These sources
establish useful ingredients; they do not certify their absence or their deployable composition.

Linux v6.17's [subreaper adoption path](https://github.com/torvalds/linux/blob/e5f0a698b34ed76002dc5cff3804a61c80233a7a/kernel/exit.c)
handles eligible orphans, and its [pidfd signal path](https://github.com/torvalds/linux/blob/e5f0a698b34ed76002dc5cff3804a61c80233a7a/kernel/signal.c)
uses a retained process identifier. Its [cgroup v2 contract](https://github.com/torvalds/linux/blob/e5f0a698b34ed76002dc5cff3804a61c80233a7a/Documentation/admin-guide/cgroup-v2.rst)
defines `populated` live-process evidence and `cgroup.kill` handling of concurrent forks and
migrations. This pinned release is a valid source reference, not a statement about the running
macOS host or an already implemented backend.

### A named guest alternative

A per-run `VZVirtualMachine` is a concrete alternative to host-tree discovery. Apple requires the
[`com.apple.security.virtualization` entitlement](https://developer.apple.com/documentation/virtualization/adding-the-virtualization-entitlement-to-your-project).
[`isSupported`](https://developer.apple.com/documentation/virtualization/vzvirtualmachine/issupported)
and configuration `validate()` must admit the actual host and configuration before untrusted
execution. This proposal has not established entitlement availability for the shipped binary,
SDK/OS support on every deployment, or working controller lifecycle there. A non-root VM process
is not categorically excluded: Apple's [raw disk attachment guidance](https://developer.apple.com/documentation/virtualization/vzdiskblockdevicestoragedeviceattachment)
recommends a separate privileged disk opener rather than running the VM as root. A file-backed
image avoids claiming that raw-device privilege is a prerequisite for every guest.

[`requestStop()`](https://developer.apple.com/documentation/virtualization/vzvirtualmachine/requeststop())
asks the guest to shut down; the request is not proof of exit. Apple specifies that
[`stop(completionHandler:)`](https://developer.apple.com/documentation/virtualization/vzvirtualmachine/stop(completionhandler:))
is destructive and completes after successful stop or an error. A selected design must require
successful completion and the stopped state, not a sent request. This supports a guest-execution
boundary only when all workload execution remains inside that VM. It does not automatically settle
host helpers, drain output buffers, complete host storage I/O or make an artifact immutable. Avoid
writable host shares and confine any necessary host helpers separately. Specify output transport,
bounded collection and storage settlement before claiming a quiescent artifact. Guest image
management, boot, CPU/memory and tool compatibility are real design obligations; no performance or
availability measurement is supplied here.

### Proposed decision and receipt wording

Do not replace initial-group cleanup with a heuristic whole-host kill sweep. First expose actual
scope and lack of certification; keep acceptance 6 open. A complete sweep may be considered only
after an independently reviewed membership-admission, identity-binding and settlement design is
selected. It must establish the run root before execution, retain admitted member provenance,
observe stop completion and settle every process it stopped within finite bounds. On abort it must
not leave a process stopped indefinitely or resume a replacement process. It cannot fall back to a
bare PID or an expired numeric PGID. Missing ancestry or an erased marker lowers coverage.

Those prerequisites may remain unsatisfied for a macOS daemon. That does not prohibit every narrow
safe cleanup action: a direct child held in unreaped parent custody can be signalled and waited;
an initial group can be signalled only while a proven live guard prevents group-ID reuse and the
intended membership is separately established. These actions do not certify escaped descendants.
A diagnostic census alone does not authorize signalling another process. Ratification must choose
between limited cleanup without certification, rejecting certified execution on this backend, or
admitting a different lifetime boundary. It must not promise that best-effort complete sweeping will
always become available.

The proposed additive `process_cleanup` field separates scope, observation completeness, positive
observations and certification. For an unobserved descendant census on the current backend:

```json
{
  "process_cleanup": {
    "scope": "initial_group",
    "observation": "not_attempted",
    "seen_alive": false,
    "certification": "unverified",
    "detail": "Detached descendant termination is not certified on this backend."
  }
}
```

`observation` concerns the bounded descendant census, not whether the direct child was waited. Its
closed proposed values are `not_attempted`, `complete` and `incomplete`. `complete` says the specified
bounded observation completed; it does not assert complete run membership. Unreadable metadata,
partial enumeration, unavailable identity binding, unacknowledged stops or exhausted bounds produce
`incomplete`. Independently, `seen_alive: true` preserves any observation of an admitted live member,
including when another part of the census is incomplete or that member later exits. It is never
cleared to hide a positive observation. `false` means no positive observation was recorded, not
that descendants do not exist. Historical sightings and current settlement detail must not be
confused.

The only proposed certification values are `unverified` and `certified_none`. The latter requires
a non-bypassable membership boundary and evidence that it is empty at the receipt boundary; none
of the evaluated current Seatbelt mechanisms establishes it. A failed signal or accepted kill
request alone is not exit evidence. Older receipts without the field remain uncertified.
`timed_out`, the direct-child exit status and execution success do not imply descendant termination.
A consumer requiring certification must explicitly refuse the unsupported backend before launch;
a tool name is not an admission policy.

### Capture policy and ratification choice

The no-follow, pre-launch-directory-handle option in #3291 is already implemented:
[`CaptureRoot::open`](../../crates/khive-pack-exec/src/capture.rs) pins the directory before launch;
component-relative `openat` uses `O_NOFOLLOW` and compares opened identities. Keep this implementation
and Amendment 11's root verification and bounded output collection. A later pathname walk would
regress that boundary. Capture is not an atomic filesystem snapshot, and a pinned directory does
not freeze file contents. An unknown or known writer can mutate it during collection.

The recommended publication policy is uniformly provisional for uncertified runs. Add
`tree_quiescence: "unverified" | "certified"` independently of `tree_capture`, reporting
`unverified` on the current backend. Under this proposed policy, publish bounded collected artifacts
with that qualification regardless of whether observation was not attempted, complete or incomplete,
and regardless of `seen_alive`. A survivor observation does not selectively remove `tree_out`,
changed entries or execution success. Amendment 11's actual root/capture errors and output limits
still govern capture degradation and success. `tree_capture: complete` means collection completed;
it does not mean every writer exited or the published bytes constitute a final immutable tree.
A consumer needing a final quiescent artifact must reject unverified publication. This explicitly
accepts that published data can be provisional or change after collection.

The alternative for ratification is uniformly certified-only publication: withhold all collected
artifacts for every run lacking certification, even when no census ran or a completed census found
nothing. It would also require an explicit pre-launch refusal policy for consumers requiring
certified execution on the current backend. It removes provisional output availability without
creating termination proof. Do not mix these policies so that skipping observation publishes while
observing a survivor loses output. Ratification must explicitly select one uniform policy; this
Proposed amendment recommends provisional publication and does not record a signed selection.

### Refutation of this recommendation

Truthful fields and uniform provisional publication do not stop a scrubbed double-fork orphan.
CPU use and writes in an allowed run directory can continue after return, and collected files can
mix content from different times. A private signal API does not cure incomplete membership,
unsupported nonchild settlement or unavailable permissions. The two-syscall Seatbelt rule is
insufficient because measured spawn attributes bypass it. A coalition terminate request is not a
kill operation. A VM shutdown request is not completed stop. None should be presented as a
convenient proof of acceptance 6.

The strongest objection to the recommendation is that availability of provisional artifacts is
inadequate for callers requiring complete lifetime containment. Those callers must refuse current
certified execution, choose the uniform certified-only publication policy or fund a separately
admitted kernel/guest boundary. That changes availability, tool compatibility and operating costs.
A fork-free profile could remove descendants but also helper-based tools; compatibility and all
creation routes would still need proof. A heuristic fixed-point census, mutable marker or bare-PID
birth-time check is rejected as certification. These tradeoffs remain explicit; #3631 and #3291
are not closed by receipt wording.

### Implementation and acceptance hold

No production change follows from this Proposed amendment. Ratification must select the uniform
publication policy, supported OS/SDK range, membership/identity/settlement primitives, actual
permissions, finite budgets and certified-policy admission surface. Membership and identity
prerequisites may prevent complete best-effort cleanup indefinitely on a deployment; select narrow
safe actions or reject certified execution rather than weakening them. A certified backend also
needs a complete boundary proof. Dependent source work waits for an authenticated signed decision.
The reported platform probes are not complete containment evidence; all new deployment, liveness,
compatibility, overhead and certification checks remain outstanding.

Receipt-only work, if ratified, needs separate tests for the closed vocabulary, preservation of
`seen_alive` under incomplete observation, historical receipts, durable lookup/listing and the
chosen uniform publication policy across unobserved, complete, incomplete and survivor cases.
Execution success must continue to describe the direct child and actual capture outcome under that
chosen policy. Such tests cannot satisfy descendant liveness. Liveness/control gates are conditional
on an actually selected containment implementation, not the addition of receipt fields.

That later gate must retain Amendment 11's output-bound and file/network confinement obligations.
The existing survivor fixture describes current lifetime behavior; a stronger backend must retain
the denial witnesses and unsandboxed controls when reanchoring its lifecycle. Add receipt-boundary
liveness cases for direct `setsid`/`setpgid`, both spawn escape attributes, fork/vfork/daemon routes,
a scrubbed double-fork orphan, marker erasure and exit/exec during identity admission. A PID-reuse
arm must prove that an unrelated replacement receives no signal; rereading a birth time is not that
proof. Include inaccessible/partial census with a positive sighting retained, STOP not acknowledged,
budget exhaustion, supervisor failure and known/unknown writers with the same publication policy.
A selected coalition backend must prove admission and empty notification, not merely terminate
request success; a guest backend must distinguish shutdown request from completed destructive stop
and settle output/storage. Independently removing selected cleanup must fail a named liveness
assertion after a normal target compiles and passes with nonzero selection. Removing identity
admission must fail the no-signal-to-replacement assertion. Compiler/setup failures and empty
selections prove neither. Existing capture mutation witnesses must continue to distinguish
no-follow identity reads from pathname substitution. All such evidence remains outstanding.

### Ratification (2026-10-03)

Ratified by the maintainer. This block governs where the text above says Proposed, not ratified or
not selected.

Scope: ADR-181's posture is that the agent's tool set is khive verbs, the shell exists only inside
one verb, and a run has no network, no home directory and no credentials. Amendment 11's arm shows
that a detached survivor keeps every one of those denials. Acceptance 6 is therefore a hygiene and
resource target, not a breach of that posture, and a heuristic sweep is not presented as
certification.

1. Publication: uniform provisional. On the Seatbelt backend `tree_quiescence` is `unverified`, and
   bounded artifacts are published with that qualification regardless of the observation outcome or
   `seen_alive`. The certified-only alternative is rejected: it withholds output and creates no
   termination proof.
2. Backend: initial-group cleanup is kept, with the receipt fields below; acceptance 6 stays open.
   The `SYS_setsid`/`SYS_setpgid` Seatbelt denial does not ship: libc `daemon()` fails under it and
   spawn attributes bypass it, so it costs compatibility without providing certification. No guest
   or coalition backend is selected; either returns as its own ADR carrying the boundary proof this
   amendment lists.
3. Receipt vocabulary: accepted as proposed, additive and closed: `process_cleanup` with `scope`,
   `observation` (`not_attempted`, `complete`, `incomplete`), `seen_alive`, `certification`
   (`unverified`, `certified_none`) and `detail`, plus `tree_quiescence`. `certified_none` is
   reserved: a producer-side test asserts that the Seatbelt backend never emits it, so the field
   cannot become a claim before a backend earns it.

Implementation released by this ratification is receipt-only, with the tests this amendment lists
for the receipt vocabulary, `seen_alive` preservation, historical receipts and the uniform
publication policy, plus the reserved-value test above.

## Amendment 13 (2026-10-21): an outbound network declaration on exec registrations

**Status: Proposed.** Partial amendment. It amends, for registrations that declare network only:
Decision step 3 ("no network"), Acceptance 4 as restated by Amendment 11, the posture sentence in
Amendment 12's Ratification scope ("a run has no network"), Amendment 1 item 5 (one new refusal
class), Amendment 1 item 7 with Amendment 7 (two additive `sandbox` fields), Amendment 2 item 3
(`exec.identity` reports a second template digest), and, for honoured runs only, Amendment 9 item 2
with the Consequences line that rests on arm 4. It amends ADR-180 Amendment 1 item 1 (the grant pin
inputs) for one property, and ADR-180's Approval fast path for rows that carry that property. It
adds one host setting, off unless configured, without which no declaration is honoured. Every
other decision in this record and in ADR-180 stands. A
registration that does not declare network runs exactly as before, and its pin and grants are
unchanged byte for byte.

This amendment provides the network declaration that a host needs to run adapters which reach a
remote service under exec. It does not decide which registrations may declare it.

### Context

A registered tool that talks to a remote service, such as a deployment CLI or a package registry
client, cannot run under exec today. The profile allows no network operation
(`khive-pack-exec/src/sandbox.rs:9-10`), and the renderer takes no network input:
`render_profile(run_dir, read_roots, never)` (`sandbox.rs:168`). Run without network, such a tool
fails with timeouts and resolution errors that read as the remote service's fault, not the policy's.

The policy surface already distinguishes how a run was allowed. `tool.check` answers with a
`source` of `grant`, `policy` or `default`, and exec records it on the receipt
(`khive-pack-exec/src/handlers.rs:1048-1052`). A grant matches only while its pinned registry id
and definition digest equal the row's current ones (ADR-180 Amendment 1 item 2). The digest covers
four properties, and a missing one is written into the pinned bytes as an explicit null
(`khive-pack-tool/src/pin.rs:11-24`). A grant whose tool name resolves to no registration when it
is granted carries a null pin and cannot authorize a registered row: a pinned row is matched only
by a grant carrying its pin (`khive-pack-tool/src/policy.rs:387-405`). A matching registration in
the grant's own namespace invalidates it permanently (the registry-insert trigger,
`khive-pack-tool/sql/grant-invalidation.sql:7-23`; test
`wildcard_grants_record_registrations_that_precede_the_approval`,
`khive-pack-tool/tests/grant_pins.rs:304`). A matching registration in another namespace makes it
inapplicable to that row without invalidating it, so it still answers for matching names that
remain unregistered in its own namespace. A name ending in `*` is also a literal name: a tool
registered as `family.*` is resolved by a grant for `family.*`, which then carries that row's pin.
So for exec, which runs only registered tools, `source` `grant` means an approved grant whose actor
and tool patterns match the request and whose pin equals the row being run. It does not mean an
exact tool name, and it does not mean an exact actor: an actor pattern such as `agent:team:*`
matches every actor label under it.

Two preflight details matter below. The check that the request's `actor` equals the caller is
skipped for an anonymous token (`handlers.rs:1025-1031`), so an anonymous caller can name any actor
and is decided against that actor's grants. And on the local daemon the caller is the actor label
on the request frame: the connection copies the frame's `actor_id` into the request identity
(`khive-runtime/src/daemon/connection.rs:405-450`), and any non-blank label becomes a non-anonymous
actor (`khive-runtime/src/actor_identity.rs:18-22`). ADR-096 binds each connection to the daemon's
own uid and states that same-uid callers form one trust domain, in which the frame's actor is
attribution and not a boundary. A same-uid client can therefore send any actor label, and the
equality check holds that label to the request's `actor`, not to a verified principal.

What the platform does was measured before writing the rule set (macOS 27.2, `/usr/bin/curl` and
`/usr/bin/nc`, this record's template written out by hand from `render_profile` at main `76717299`, rules appended;
every target answered from an unsandboxed process as the control):

| profile                                                              | remote https  | v4 loopback | v6 loopback | host's own LAN and VPN addresses | unix socket under `/private/tmp` | listen  |
| -------------------------------------------------------------------- | ------------- | ----------- | ----------- | -------------------------------- | -------------------------------- | ------- |
| template today                                                       | refused       | not run     | not run     | not run                          | not run                          | not run |
| plus `(allow network-outbound)`, no resolver reads                   | no resolution | not run     | not run     | not run                          | not run                          | not run |
| plus `(allow network-outbound)` and resolver reads                   | reached       | reached     | reached     | not run                          | reached                          | refused |
| plus `(allow network-outbound (remote ip "*:*"))` and resolver reads | no resolution | reached     | not run     | not run                          | refused                          | not run |
| the same plus `(deny network-outbound (remote ip "localhost:*"))`    | no resolution | reached     | not run     | not run                          | refused                          | not run |
| the rule set in item 5                                               | reached       | refused     | refused     | refused                          | refused                          | refused |

Every cell is a TCP connection, a unix stream-socket connection, or a TCP listen on `127.0.0.1`.
The v6 loopback cell is `::1`, and the host's own addresses sampled were one LAN and one VPN IPv4
address. No UDP, ICMP or raw socket, IPv4-mapped IPv6 address, IPv6 global or link-local address of
the host, multicast group, or IPv6 bind or listen was run.

Three readings follow. A bare outbound allowance reaches every loopback service on the host and any
unix socket the run can name, which is the host's own daemons rather than the internet. Name
resolution needs the resolver's unix socket; an address-only rule resolves nothing. And the
`localhost` deny took effect in the last row and had no effect in the row above it, which differ only by
the resolver socket allowance; this was repeated and is not explained. The rule set is therefore
accepted by its driven arms, never by its text.

### Decision

1. **One registration property, `network`, with one value.** A registry row either has no `network`
   property, and runs with no network as today, or has `network: "outbound"`. `tool.register`
   accepts `network` only with the value `"outbound"` and only with an `exec:` source, since no
   other executor reads it; any other value, including null, and `network` on any other source are
   refused with the reason named. There is no host or port list. The Seatbelt rules below name `*`
   and `localhost` as hosts and nothing narrower, so a destination list would read as enforced when
   it is not. Per-destination egress is later work and is not decided here.

   Re-registering an existing name keeps the stored row unchanged (ADR-180 test
   `re_registration_keeps_the_approved_object_and_definition`), so a `network` passed on a
   re-registration does not reach the row. `tool.describe`, `tool.list` and the register response
   report the stored `network`, so the caller sees what is stored rather than what was asked.

   Which registrations may carry `network` is the hosting deployment's registration policy, not
   exec's. exec enforces the declaration a row carries; it does not decide eligibility, and nothing
   here promises that a deployment's general code runs without network. Whether the host runs
   networked rows at all is a separate host setting, item 3.

2. **`network` joins the grant pin when the row carries it.** This amends ADR-180 Amendment 1
   item 1 for one property. When the row's properties contain the key `network`, the pinned inputs
   contain `network` with the stored value, whatever that value is. When the key is absent, the
   pinned inputs are the four properties exactly as today, with no `network` key.

   This is the one exception to `pin.rs`'s explicit-null rule, and the reason is the upgrade. Under
   that rule a row without the key would gain `"network": null` in its pinned bytes, its digest
   would change, and every grant written before the change would stop being active at once, for
   rows nobody changed. With the exception, a row without `network` keeps byte-identical pinned
   inputs and keeps its grants. A row that gains the key, by any path, has a different digest, so a
   grant approved for the row without network does not cover it and the owner grants again. No verb
   adds the key to an existing row today (item 1); the pin covers the paths ADR-180 Amendment 1
   item 4 names: a restore, a second writer inside the owning pack, a later update verb.

3. **The declaration is honoured only on an enabled host, by a grant to a non-anonymous actor.** A
   run of a row carrying `network` proceeds only when all of these hold, checked in preflight after
   the policy decision and before the binary digest:
   - the host has enabled networked runs;
   - the stored value is `"outbound"`;
   - the decision is `allow` with `source` `grant`;
   - the token is not anonymous.

   The host setting is part of the daemon's configuration, read when the daemon starts, and is off
   unless the configuration turns it on; no verb changes it. It is checked at run time, not at
   registration, so it holds for every row carrying `network` however the row was written,
   including a restored row or one written by another writer.

   For a non-anonymous token the request's `actor` must also equal the caller. That check exists
   today and runs first in preflight, before registration and policy (`handlers.rs:1025-1032`); it
   keeps its position and its reason, and this amendment relies on it without moving it. A
   `policy` or `default` allow never opens network. The anonymous condition closes the path in the
   Context: without it, an anonymous caller naming another actor would be decided against that
   actor's grant and run with its network. A caller-supplied actor may be removed in general later;
   this condition is exec's own floor and does not wait for that.

   A non-anonymous actor is an attributed actor, not an individually proven grant holder. On the
   local daemon a same-uid client can send the label of an actor that holds a grant, as both the
   frame's actor and the request's `actor`, and meet every condition above (Context). Within
   ADR-096's single-uid trust domain that is the accepted model: the grant's actor condition
   separates attributed actors and does not keep one same-uid client from another. Binding a run to
   an individually verified holder needs the connection-identity mechanism that ADR-096 defers to
   its own record. Until a transport supplies one, the deployment limits network reach through
   which registrations may declare it (item 1) and through the host setting, not through the
   grant's actor. The daemon cannot tell how many principals share its uid (ADR-096; the
   connection check is same-uid, not same-principal, `khive-runtime/src/daemon/boot_guard.rs:347-354`), so the host
   setting is the operator's statement that one principal does. A deployment in which more than one
   principal shares the daemon's uid, such as any multi-tenant or hosted service, leaves the setting
   off until the connection-identity mechanism is accepted, implemented and binds the request
   principal; with it off, every row carrying `network` is refused (item 4).

4. **A run that would not be honoured is refused, never run without network.** When the row
   carries `network` and any condition in item 3 fails, exec writes a refusal receipt (Amendment 1
   item 5): `decision` as decided, `tree_out` null, no run directory, `sandbox` null (Amendment 7),
   and a `reason` that starts with `network_unhonoured:` and names the failed condition (the host
   setting, the decision source, or an anonymous caller). A stored value other than `"outbound"` is refused the
   same way with the prefix `network_invalid:`. A non-anonymous caller whose request names another
   actor is refused earlier by the existing identity check, with its existing reason and neither
   prefix. Refusing is never weaker than running without network, and the reason names the real
   cause instead of a timeout inside the tool.

5. **An honoured run gets outbound rules only, under its own template.** The profile is this
   record's template with these lines appended:

   ```text
   (allow network-outbound (remote ip "*:*"))
   (allow network-outbound (remote unix-socket (path-literal "/private/var/run/mDNSResponder")))
   (deny network-outbound (remote ip "localhost:*"))
   (allow file-read-metadata (literal "/etc") (literal "/var") (literal "/private/var/run"))
   (allow file-read* (literal "/private/var/run/resolv.conf"))
   ```

   Nothing allows bind, listen or inbound traffic, so the template's default deny keeps refusing
   them. The renderer takes the honoured decision as an input; the outbound template has its own
   template digest, computed the way `template_digest` computes today's (`sandbox.rs:237`).
   `exec.identity` keeps `profile_template_digest` for the template without network and adds
   `profile_template_digest_outbound`. The receipt's `profile_digest` already digests the profile
   that ran, so every receipt names which one it was.

6. **The receipt names the declaration and the profile separately.** The `sandbox` object gains
   two additive fields (Amendment 7): `network_declared`, read from the registry row, and
   `network_honoured`, read from the template the renderer used. Under this amendment a prepared
   sandbox always has them equal, since an unhonoured declaration is refused before the sandbox is
   prepared. They are kept as two fields because they come from two sources, and their equality is
   an arm: a renderer that ignored the decision would show up as a mismatch.

7. **A request for a networked row creates a request row unless a grant already allows it.** This
   amends ADR-180's Approval fast path for rows carrying `network` only. `tool.request` returns the
   decision without a row when it is already `allow` (`khive-pack-tool/src/handlers.rs:930-938`),
   and a registration with `side_effect: "read"` defaults to `allow`, so under that contract a
   networked row allowed by `policy` or `default` could never obtain the grant item 3 requires. For
   a registered row carrying `network`, `tool.request` inserts a request row and returns its id
   whenever the decision is not `allow` with `source` `grant`. The row is the one the reference
   resolves to, by name, id or prefix, as exec resolves it (`khive-pack-tool/src/handlers.rs:262-269`),
   not the spelling of the reference. The inserted request follows ADR-180's Approval contract
   unchanged: it is created `requested` with a null pin, approval pins it, the self-grant refusal
   and status transitions apply, and `notify` is sent as for any other request. The response still
   carries the decision, so the caller sees both that the tool is allowed and that a run with
   network waits for the grant. For every other row the fast path is unchanged.

### What a grant on a networked registration approves

The grantor should read these as the approval's actual scope.

1. **Every routable address that is not the host's own.** `*` admits other hosts on private
   networks and VPN peers the host can route to. That was not measured, and it holds by
   construction. The intended rule is that a run reaches none of the host's own addresses, by any
   transport or address form. What is measured is narrower: TCP connections to v4 and v6 loopback
   and to one LAN and one VPN IPv4 address of the host are refused. That refusal is held by arm 51,
   not by a rule that names those addresses: the template denies `localhost` only, and why that
   also refuses the host's LAN and VPN addresses was measured, not explained. UDP, ICMP and raw
   sockets, IPv4-mapped IPv6 forms of the host's addresses, the host's IPv6 global and link-local
   addresses, and IPv6 and UDP bind and listen were not run. Arm 59 names each of these classes as
   an operation with its own unsandboxed control, and a host enables networked runs (Decision
   item 3) only after arm 59 has run every class on that host's macOS version. A class that was not
   run is not treated as refused. A multicast datagram is delivered by default to the host's own
   members of the group as well as to other hosts, so it is an own-address path the `localhost`
   rule does not name; arm 59 records it rather than assuming either outcome.
2. **Anything the run can read can leave.** The existing data read set is retained: the
   materialized tree, the configured read roots, the system read roots, and the values of
   allow-listed environment keys the caller supplied. Item 5 adds the resolver configuration and the
   exact metadata reads it lists. A grant for a networked tool approves sending any of them
   anywhere.
3. **Code the tool fetches runs, and remotes are in reach.** The profile allows process execution
   except the denied set (Amendment 9), so a binary the tool downloads into the run directory runs
   under the same profile. Materialized tree content can already do that; network adds code whose
   origin is outside the store. Amendment 9 item 2 does not hold for an honoured run: such a run can
   act on remote services, remote repositories included, through any program or interpreter the
   by-path denial does not match, and only that by-path denial remains. A run of a row without
   `network` keeps Amendment 9 item 2 unchanged, and arm 4 stays required for it.
4. **Outbound reach can outlive the receipt.** A descendant that leaves the initial process group
   keeps the profile it was launched under (Amendment 11), so a detached descendant of an honoured
   run keeps outbound reach after `exec.run` returns. Amendment 11's survivor arm keeps its refused
   outbound connection for runs without network; for an honoured run the survivor's write and bind
   denials still hold and its outbound reach does not end. This stays open with the process-lifetime
   design (#3631).
5. **The resolver daemon's socket is reachable, not only its lookups.** Lookups the run makes are
   sent by the host's resolver over its unix socket, the one unix socket the run may reach. The rule
   grants the socket, not a lookup-only subset of what the daemon serves: its other operations,
   such as service browsing and registration, are reachable as far as the daemon itself allows them,
   and were not measured. Separately, the template already allowed a mach lookup of the directory
   service (`com.apple.system.opendirectoryd.libinfo`, `sandbox.rs:220`) before this amendment; what
   that service does on a run's behalf is its own policy, also not measured here.
6. **Measured with one client.** The rule set was measured with `/usr/bin/curl`. A tool that checks
   certificates through a platform service it reaches by a mach lookup is not covered; adding that
   service is a template change with a new template digest and its own arm.
7. **Other backends.** A backend other than Seatbelt must enforce the declaration as stated here or
   refuse a run that carries it. It never ignores it.

### Acceptance

The network arms (50 to 56, and 59) run on macOS and pass only with each operation's unsandboxed control
succeeding in the same run; an arm whose control fails is reported as not run, never as passed. An
arm that needs something the host lacks (no egress for 50, no non-loopback address for 51) is also
not run, not passed. They run again on every macOS version the daemon supports, because the rule
set is accepted by these arms and not by its text. Every arm except 60 runs with the host setting of
Decision item 3 on.

Pin and registration:

43. A row without `network` has pinned bytes equal to the bytes the four-property function produces
    (a golden fixture computed before the change), and a grant written before the change still
    answers `allow` from `grant` after it. Mutation control, stated before running: writing
    `"network": null` into the pinned inputs of a row without the key turns this arm red.
44. With an active grant on a row without `network`, writing `network: "outbound"` onto the row
    through the same storage write the ADR-180 pin tests use makes `tool.check` answer from policy
    or default, and `exec.run` refuses. A new grant on the changed row answers `allow` from `grant`
    with the new digest. Mutation control: leaving `network` out of the pinned inputs turns this arm
    red.
45. `tool.register` accepts `network: "outbound"` with an `exec:` source and refuses `network` set
    to null, `"inbound"`, `"OUTBOUND"` and `true`, and `network` on an `mcp:` source, each with the
    reason named. The accepted case is the control.

Decision path (each refusal arm asserts the `network_unhonoured:` or `network_invalid:` prefix,
`sandbox` null, `tree_out` null and no run directory; each has as its control the same row run by a
non-anonymous caller that names itself as `actor` and holds a grant pinned to the row):

46. A row carrying `network` allowed from `policy` is refused.
47. A row carrying `network` allowed from `default` is refused.
48. An anonymous request naming an actor that holds the grant, sent over the daemon transport with
    no actor on the frame, is refused. Mutation control: removing
    the anonymous condition turns this arm red and leaves 46 and 47 green.
49. A stored `network` value other than `"outbound"`, written through storage, is refused with
    `network_invalid:`, and the pin includes the stored value (its digest differs from the same row
    without the key).

An honoured run (a grant pinned to the row, a non-anonymous caller naming itself):

50. Resolves a public name and connects to a remote HTTPS endpoint named by the test configuration.
51. Is refused a connection to a v4 loopback listener, to a v6 loopback listener, and to a listener
    on each non-loopback address of the host. Mutation control: removing the `localhost` deny line
    turns this arm red.
52. Is refused a connection to a unix socket under the run's temporary directory and under
    `/private/tmp`.
53. Is refused bind and listen on a TCP port on `127.0.0.1` and on `0.0.0.0`.
54. Records `network_declared` and `network_honoured` both true, and a `profile_digest` whose
    profile re-renders from the outbound template; `exec.identity` reports two different template
    digests; and the rendered outbound template equals a golden text fixture line for line.
    Mutation controls: rendering the template without network for an honoured run turns 50 and this
    arm red, and adding any rule to the outbound template turns this arm red.
55. A row without `network`, granted, runs under the template without network: both fields false,
    outbound connect refused as in Acceptance 4. Mutation control: rendering the outbound template
    for every allowed run turns this arm red.
56. A detached descendant of an honoured run, alive after the receipt, is still refused a write
    outside the run directory and a bind, and still reaches the remote endpoint. This arm records
    item 4 of the scope above as current behavior; it is not a containment claim.

Identity, approval, the remaining address forms and the host setting:

57. A non-anonymous caller B sending a request whose `actor` is A, where A holds an active grant
    pinned to the row, is refused before the row is materialized, with the existing identity reason
    and neither prefix of Decision item 4. Control: the same row run by A. Mutation control:
    deleting the equality check in preflight turns this arm red and leaves 46 to 49 green.
58. For a row carrying `network`, starting once from a `policy` allow and once from a `default`
    allow: exec refuses with `network_unhonoured:`; `tool.request`, given the row by name and once
    by id, returns a non-null request id with no change to policy, and with `notify` set it sends
    the notice as for any other request; a different actor grants it; exec then runs honoured.
    Mutation controls: restoring the fast path for rows carrying `network` turns this arm red, and
    so does skipping `notify` for the inserted request.
59. An honoured run is refused, for each address below that the host has:
    - a UDP send and a TCP connection to v4 and v6 loopback, to each non-loopback IPv4 address of
      the host, to the IPv4-mapped form of each of those (`::ffff:127.0.0.1` included), and to each
      IPv6 global and link-local address of the host, link-local with its scope;
    - a TCP bind and listen, and a UDP bind, on `::1` and `::`, and a UDP bind on `127.0.0.1` and
      `0.0.0.0`;
    - creating a raw socket (`SOCK_RAW`, IPv4 and IPv6, ICMP protocol) and sending on it, and an
      ICMP echo through a datagram ICMP socket (`SOCK_DGRAM`, `IPPROTO_ICMP`), each to the host's own
      addresses.

    A multicast send to a group the host has joined is run and its outcome recorded. Each operation
    has its unsandboxed control in the same run, made with the same call, so a failure to create the
    socket, a missing privilege or a failed setup bind is never read as the profile's refusal. An
    operation the unsandboxed control cannot perform at the daemon's uid, such as a raw socket
    without the privilege, is recorded as unavailable to that uid; the profile only narrows what the
    uid can do, so such an operation is closed for runs, and the arm is run again for it if the
    daemon's privileges change. Any other class not run, and a multicast outcome other than refused,
    keeps the host setting off on that host until the class is run or an accepted scope change
    covers it.
60. With the host setting off, a row carrying `network: "outbound"`, a grant pinned to it and a
    non-anonymous caller naming itself as `actor` is refused with `network_unhonoured:` naming the
    host setting, before the binary digest and with no run directory; the same holds for such a row
    written through storage. Control: the same row and caller with the setting on runs honoured.
    Mutation control: deleting the host-setting check turns this arm red and leaves 46 to 49, 57
    and 58 green.

Implementation follows acceptance of this amendment and goes through the usual review and gates.
