# Scratch-clone cache — design notes

Long-form rationale extracted from `crates/khive-pack-git/src/cache.rs`
doc-comments (ADR-088 Amendment 1, remote-URL mode for `git.digest`).

## Module overview

Clones/fetches into `~/.khive/scratch/git-digest/<cache_key>/`, keyed by
canonical URL (`crate::source::cache_key`). An LRU cap evicts the
least-recently-used clone (by a `.khive-last-used` marker file's mtime,
touched on every successful `ensure_clone`) once the cache exceeds
`digest_cache_max_repos` entries or `digest_cache_max_bytes` total size —
eviction is safe because ingest cursors live in the database, not the clone.
Eviction only ever removes entries it can _prove_ it owns (`is_owned_entry`:
a 16-hex cache-key directory name containing both a `.git` dir and the
`.khive-last-used` marker) — a `KHIVE_GIT_DIGEST_SCRATCH_ROOT` override
pointed at a broader or pre-existing directory must never lose unrelated
operator data.

A per-clone size cap (`digest_cache_clone_max_bytes`) rejects a clone/fetch
that grows past its own budget _before_ it ever enters the addressable cache
slot: `ensure_clone` clones/fetches into a private staging directory under
the cache root but outside every addressable `<cache_key>` slot, measures it,
and only moves it into `<root>/<cache_key>/` when it is under the cap. A
too-large clone is deleted from staging and never
touches `evict_lru`'s bookkeeping or the cache slot. This guarantees the cap
is enforced before the clone enters the cache — it does NOT bound the
transient disk usage of the clone/fetch child process itself while it runs
in staging (`git` has no reliable pre-flight or mid-transfer size check for
a partial `--filter=blob:none` clone); a single oversized `git clone` can
still transiently consume disk in the staging directory before this check
rejects and removes it.

Config is env-var driven today (`KHIVE_GIT_DIGEST_CACHE_MAX_REPOS`,
`KHIVE_GIT_DIGEST_CACHE_MAX_BYTES`, `KHIVE_GIT_DIGEST_CLONE_MAX_BYTES`,
`KHIVE_GIT_DIGEST_SCRATCH_ROOT`) rather than a `[git]` TOML section.

`ensure_clone`/`refetch_clone`/`reclone` each check a slot's state and then
mutate it based on that check. A per-`cache_key` advisory `slot_lock` (issue
#805) is held for the full span of each of those functions, so two calls
racing the same slot can never interleave their check-and-mutate sequences.
Calls against distinct keys run their clone/fetch work concurrently; only
the short cache-wide `evict_lru` pass is serialized (by a separate
process-wide `EVICTION_LOCK`), and that pass defers — rather than blocks on
— any candidate whose per-slot lock is currently held. See `slot_lock` and
`evict_lru` below.

### Private staging namespace and liveness-based reaping

Fresh clones never stage directly in `root` (which may be shared, or
`KHIVE_GIT_DIGEST_SCRATCH_ROOT`-overridden to a broader or pre-existing
directory). They stage under `<root>/.khive-git-staging/`, a subdirectory
this cache owns outright — a staging entry's canonical-UUID shape can never
collide with unrelated operator data sitting in a shared root, closing the
class of bug where a broad scratch-root override made shape-matching alone
an unsafe ownership test.

Each staging entry (`<namespace>/<uuid>/`, containing a `.khive-staging.lock`
file and a `repo/` clone destination) holds an exclusive advisory lock
(`std::fs::File::try_lock` — `flock` on unix, `LockFileEx` on Windows,
portable stable API since Rust 1.89) on that lock file for the entire span
of `install_fresh_clone`. Reaping is a liveness check, not an age check:
`reap_stale_staging` tries to acquire the same lock with `try_lock`. If it
succeeds, nothing holds it — the owning process is gone (including a
`SIGKILL`, which the kernel releases the lock for automatically) — and the
entry is abandoned. If it would block, a live process holds it, and the
entry survives no matter how old it looks; there is no documented wall-clock
bound on a digest pass, so age alone was never a sound staleness signal for
an active clone. A missing lock file (the brief mkdir-then-open-lock gap at
the very start of `install_fresh_clone`, or residue from before this design)
falls back to the old age-only check (`STALE_STAGING_AGE`, 24h) as a narrow
belt-and-suspenders case — this cannot false-positive against a live clone,
since a live clone writes its lock file within microseconds of creating its
staging directory, well before `git clone` itself starts.

`prepare_cache_root` runs this sweep before every public cache mutation, but
throttled to at most once per `REAP_THROTTLE_INTERVAL` (5 minutes, tracked
by a `.khive-last-swept` marker file's mtime) rather than on every single
mutation — once the namespace is clean, a full scan+liveness pass on every
`ensure_clone` call is unbounded latency for no benefit.

## `CacheError::UnsafeToReplace`

A repair operation (refetch/reclone) would have to touch a path that does
not prove itself an owned cache slot (`is_owned_entry`) or is not a direct
child of the scratch root — refused rather than risking deletion of
unrelated operator data under an overridden `KHIVE_GIT_DIGEST_SCRATCH_ROOT`.

## `ensure_clone`

An existing path at the cache-key slot is only ever treated as a fetchable
cache slot when it already passes `is_owned_entry` — a `.git` directory
sitting at that path without the `.khive-last-used` marker (a foreign
directory that happens to collide with the cache key, or a directory a
crashed prior run left in a pre-`touch` state) is refused with
`CacheError::UnsafeToReplace` rather than fetched into or adopted (issue
#765). A fresh clone is written into a private staging directory first
(`git clone --filter=blob:none`), measured there, marked with
`.khive-last-used` there, and only _moved_ into the addressable
`<root>/<cache_key>/` slot once it is under the cap and already carries its
ownership marker — an oversized clone never enters the cache slot, never
participates in `evict_lru`'s accounting, and is removed from staging
immediately; a process interruption between the clone and the rename can
never leave a live, markerless slot behind.

A repo that grew past the per-clone cap since it was last fetched is
evicted from the cache slot on the spot, through the same
ownership-guarded `remove_owned_entry` every other repair path uses,
propagating any cleanup/ownership failure instead of discarding it.

Runs LRU eviction over the rest of the cache after a successful
clone/fetch (this clone is exempt from its own eviction pass).

## `refetch_clone`

Re-fetches a corrupt-but-present cache slot with `git fetch --refetch`
(issue #765): downloads a complete fresh filtered packfile rather than
trusting the existing (possibly promisor-incomplete) object store,
repairing a partial/pruned clone in place. Only ever operates on an
existing slot — callers repair a slot only after a prior `ensure_clone`
already produced one.

Re-checks `is_owned_entry` immediately before fetching (issue #765
follow-up PR #788): the gap between `ensure_clone`'s own ownership check
and this repair running — project resolution and GitHub ingestion happen
in between — is wide enough for the slot to go markerless or be replaced,
so this function cannot rely on the caller having checked recently.
Concurrent same-key mutation is separately excluded by `slot_lock` (issue
#805, see below): a per-`cache_key` advisory lock held across the whole
check-and-mutate span of `ensure_clone`/`refetch_clone`/`reclone`, so two
calls racing the same slot cannot interleave. That lock does not close the
staleness window this re-check addresses — it is held only for a single
call's own span, not across the caller's earlier `ensure_clone` (released
before project resolution and lengthy GitHub ingestion run) — so the slot
can still go markerless in that intra-request interval, and the re-check is
what narrows it.

The over-cap cleanup path routes through the same ownership-guarded
`remove_owned_entry` `reclone` uses, rather than a raw `remove_dir_all` — a
repair primitive must never delete a path that doesn't prove itself an
owned cache slot, even on the cap-exceeded cleanup path. A cleanup/ownership
failure is propagated instead of discarding it.

## `reclone`

Evicts an owned cache slot (if present) and installs a fresh clone in its
place (issue #765's fallback when a refetch cannot repair the slot).
Refuses via `CacheError::UnsafeToReplace` when the existing path does not
prove itself an owned cache slot — the same ownership guard `evict_lru`
uses.

## `slot_lock`

Returns the per-`cache_key` advisory `Mutex` from a process-global registry
(issue #805), creating it on first use. `ensure_clone`, `refetch_clone`, and
`reclone` each hold this lock for their whole check-and-mutate span, so two
calls racing the same slot cannot interleave a check against another call's
mutation. The lock is advisory and same-process only: it serializes this
crate's own cache mutations, not an external process touching the scratch
root. It is deliberately _not_ held across a caller's separate `ensure_clone`
and later `refetch_clone` calls within one request — that intra-request
staleness window is what `refetch_clone`'s pre-fetch ownership re-check
narrows instead.

`evict_lru` takes each candidate slot's lock with `try_lock` rather than
blocking, so a cache-wide eviction pass never waits on an in-flight same-key
mutation. How a deferred candidate is nonetheless brought back under the
caps is covered under `evict_lru` below.

## `install_fresh_clone`

Shared staging-clone-then-move path for both a first-time `ensure_clone`
and a `reclone` repair: creates a per-call wrapper directory under the
private staging namespace, opens and `try_lock`s its `.khive-staging.lock`
file (held for this whole function — see "Private staging namespace and
liveness-based reaping" above), clones into `<wrapper>/repo`, measures it
against the per-clone cap, writes the `.khive-last-used` ownership marker
into it, and only then moves it into the addressable `<root>/<cache_key>/`
slot — an oversized clone never enters the cache slot, and because the
marker is written before the atomic rename, a process interruption between
clone and rename can never leave a live, markerless slot at the cache-key
path (issue #765). The wrapper (lock file included) is removed on both the
success and every failure path.

A kill can still interrupt the process before any Rust cleanup guard runs and
leave the wrapper directory behind. `prepare_cache_root` / `reap_stale_staging`
are the cross-process recovery boundary for that case: the kernel releases
the wrapper's lock the instant the process dies, and the next sweep reaps it
regardless of age.

## `prepare_cache_root` / `reap_stale_staging` / `staging_liveness`

`prepare_cache_root` creates the scratch root and the private staging
namespace, then runs `reap_stale_staging` at most once per
`REAP_THROTTLE_INTERVAL` (see above). `reap_stale_staging` enumerates only
the namespace's direct children. A deletion candidate must be a real
directory (never a symlink, file, or nested path) whose name is exactly the
lowercase canonical hyphenated spelling of a UUID (an in-flight clone
wrapper) or `trash-` followed by one (an interrupted
`delete_verified_owned_entry`, whose renamed slot would otherwise be
unreclaimable residue); its liveness is then decided by `staging_liveness` —
`try_lock`-acquirable (or missing its lock file _and_ older than the 24h age
fallback) means abandoned, anything else means live and untouched regardless
of age. Trash residue never carries a lock file, so the age fallback alone
governs it: a fresh entry (a recursive delete still in flight) survives. Removal uses the same bounded
retry helper as owned cache eviction and tolerates another process winning
the same cleanup race; other I/O failures surface instead of silently
leaving disk growth unobservable.

## `remove_owned_entry` / `delete_verified_owned_entry`

`remove_owned_entry` removes `repo_dir` only when it is a direct child of
`root` AND passes `is_owned_entry` — refuses (`CacheError::UnsafeToReplace`)
rather than deleting anything else, including a not-yet-existing or
foreign-shaped path. A slot that does not currently exist is not an error:
there is simply nothing to remove before installing a fresh clone.

The actual deletion is `delete_verified_owned_entry`. Unlike a staging
wrapper (isolated in a namespace nothing else writes to), an owned cache
slot necessarily lives in the shared, possibly-overridden root — so a
pathname-based check-then-`remove_dir_all` leaves a TOCTOU window an
external writer could race for as long as the recursive delete takes
(potentially seconds against a large git tree). On unix,
`delete_verified_owned_entry` instead: opens `root`, then `openat`s `name`
with `O_DIRECTORY | O_NOFOLLOW` (refusing a symlink or non-directory
outright); re-verifies ownership (`.git` entry present, `.khive-last-used`
a regular file) against that fd, not a fresh pathname walk; `fstat`s it to
capture `(dev, ino)`; moves it into the private staging namespace with one
fd-relative `renameat` call; and confirms the entry that landed under the
new name has the identical `(dev, ino)` before running the (possibly slow)
recursive delete on it. This shrinks the external-writer race window from
"however long the delete takes" down to the handful of syscalls between the
`openat` and the `renameat` — POSIX has no primitive to rename "the exact
inode this fd points to" without a name lookup, so this is race-_resistant_
rather than race-proof, matching the residual-risk shape of every `rm -rf`
implementation's final directory-removal step. Non-unix targets (Windows CI
exists for this workspace) keep the prior pathname-based
`remove_dir_all_retrying` behavior — no fd-relative directory API is stable
there yet.

## `unix_fd` / `is_owned_entry_via_fd`

`unix_fd` is a small private module of `openat`/`fstatat`/`fstat`/`renameat`
wrappers bound to an already-opened directory descriptor, mirroring the
`O_NOFOLLOW`/`fstat` idiom already used by `khive-db`'s WAL-pin sidecar
(`crates/khive-db/src/walpin.rs`) and `khive-vamana`'s external-id sidecar
(`crates/khive-vamana/src/external_ids.rs`): every operation after the
initial `open`/`openat` is relative to a handle the kernel resolved once,
immune to the original pathname being swapped out from under it afterward.
`is_owned_entry_via_fd` is `is_owned_entry`'s fd-relative mirror, used by
`delete_verified_owned_entry` right before it acts.

## `remove_dir_all_retrying`

`std::fs::remove_dir_all` on a large git working tree can transiently fail
with "directory not empty" when something else briefly touches the tree
mid-removal (e.g. a filesystem indexer) — retry a few times before giving
up, rather than letting a one-off transient race abort a repair that would
otherwise succeed.

## `clone` (git subprocess): `maintenance.auto=false`

`-c maintenance.auto=false` on every clone/fetch into a cache slot, as
defensive hardening. `git fetch` runs auto-maintenance after it finishes
when `maintenance.auto` (default true) is set, and since git 2.47 that
maintenance runs as a _detached background child_
(`git maintenance run --auto --detach`) that can outlive the foreground
command; on 2.46 and earlier it ran synchronously. The spawn is
trace2-proven in both directions on the `fetch --refetch` path
(`GIT_TRACE2_EVENT`, git 2.49: with default config the child forks; with
`maintenance.auto=false` it does not). The same trace showed `clone`
spawning no maintenance child; the flag is applied to the clone builder too
purely as harmless defensive configuration, with no trace evidence claimed
for that path. When one of the detached child's tasks fires it mutates the
slot's `.git` tree (commit-graph writes, pack maintenance, lock files)
concurrently with any `dir_size`/`evict_lru` walk of the same slot. Whether
such a task actually fired in issue #842's historical macOS ENOENT failures
is not proven — in small repos the child typically finds no task to run and
exits quickly — so the load-bearing fix for that flake family is the
descendant-vanish tolerance in `dir_size`; this flag removes the one
background mutator git itself can fork into our cache slots. `gc.auto=0`
alone does **not** suppress the child (trace2-verified); it is kept
alongside because it disables `git gc --auto`'s separate opportunistic-gc
check, harmless to also turn off here.

This does not mean a cache slot is naturally garbage-collected some other
way instead: no cache-slot repo is ever gc'd or maintenance'd by us. Growth
is bounded by wholesale eviction, not in-place compaction —
`ensure_clone`/`refetch_clone` delete a slot outright (`remove_owned_entry`)
the moment it measures over `digest_cache_clone_max_bytes` after a fetch,
and `evict_lru` deletes whole least-recently-used slot directories once the
cache-wide `digest_cache_max_repos`/`digest_cache_max_bytes` caps are
exceeded. A slot can be fetched into repeatedly, but it can never
accumulate objects past its own size cap without being deleted and
re-cloned fresh, so there is nothing for git's own gc/maintenance to
usefully do in a cache slot.

## `fetch_refetch`

Issue #765 repair primitive: `git fetch --refetch origin` obtains a
complete fresh filtered packfile instead of incrementally trusting the
existing (possibly promisor-incomplete) object store — the documented fix
for a partial clone that has dropped objects it should still have.

## `io_err`

Wraps an I/O error with the operation and path it happened on — a bare
`CacheError::Io(e)` at these call sites used to surface as an opaque "No
such file or directory" with no way to tell which of the many paths
`dir_size`/`touch`/`evict_lru` touch actually disappeared.

## `dir_size`

Recursive directory size, following no symlinks (`symlink_metadata`
throughout, so a symlink itself is sized but never traversed — clones
never legitimately contain symlinked directories pointing outside the
clone, and this avoids any possibility of a symlink loop).

Tolerant of a _descendant_ disappearing mid-walk (a vanished entry beneath
an existing root contributes 0 bytes rather than aborting the whole size
computation): a cache slot's `.git` tree can legitimately be mutated by
something outside this function's control while it walks it — a concurrent
`evict_lru`/`ensure_clone` repair on the same slot, or a background `git
maintenance` child from before `maintenance.auto=false` applied to every
command this crate issues. This accounting is inherently a snapshot of a
possibly-changing tree, so "a thing under the root I was about to size is
already gone" is not an error here.

The walk **root** itself vanishing is different and is NOT tolerated — it
surfaces as `CacheError::Io(NotFound)` rather than silently sizing to `0`. A
caller that genuinely expects the root it's sizing to sometimes be absent
(rather than an existing root racing a mid-walk mutation) must check for
that error explicitly and decide its own semantics at that call site
(`evict_lru` does this for a listed entry that a concurrent repair deleted
between `read_dir` and this call); `dir_size` itself never launders a
missing root into a bare `0`, which previously let `evict_lru` report
success with a missing keep slot or count a phantom candidate and evict a
valid one unnecessarily.

## `is_owned_entry`

Whether `path` is a directory `ensure_clone` could plausibly have created:
a 16-lowercase-hex `cache_key`-shaped directory name (never a UUID staging
dir, never an arbitrary operator directory), itself a real directory
rather than a symlink (a symlink placed at the cache-key path pointing at
an unrelated owned-looking or foreign directory must never be treated as
an owned slot), containing both a `.git` entry and the `.khive-last-used`
marker written by `touch`. Eviction (and any future scratch-root cleanup)
must only ever remove entries that pass this check.

## `evict_lru`

Evicts least-recently-used clones under `root` (by `.khive-last-used`
mtime) until both the repo-count cap and the total-byte cap are satisfied.
`keep` (the clone `ensure_clone` just touched) is never evicted. Only
removes paths that are direct children of `root` AND pass `is_owned_entry`
— eviction never touches user-owned or non-cache paths.

`keep`'s own `dir_size` call is deliberately NOT tolerant of `keep`
vanishing: every caller touches (or freshly installs) `keep` immediately
before calling `evict_lru` in the same synchronous call chain, so `keep`
disappearing out from under this call is not an expected repair race — it
is either a genuine bug or an external actor deleting our slot, and
silently sizing it to `0` would let eviction report success while the slot
the caller asked to keep is actually gone. A listed _candidate_ entry is
different — another `evict_lru`/`ensure_clone` repairing the same root can
legitimately delete it between the `read_dir` listing and the `dir_size`
call, so that vanish is tolerated by skipping the entry rather than
aborting the whole pass.

A candidate whose `slot_lock` is currently held (an in-flight mutation on
that key) is deferred: `evict_lru` takes each candidate lock with `try_lock`
and skips a `WouldBlock` rather than waiting, so an eviction pass never
blocks on a concurrent clone/fetch (and, holding `EVICTION_LOCK` while a
mutation may hold a slot lock and be about to wait on `EVICTION_LOCK`, must
not). A deferred candidate is therefore _not_ counted in this pass — so this
pass alone can return with the caps still exceeded. The guarantee that the
caps are nonetheless restored is `enforce_caps` (below): every mutation runs
a cap pass on its own exit, so the last of a set of concurrent mutations to
release its lock sees the others unlocked and enforces the caps over the
settled set.

`evict_lru` and `enforce_caps` share one core (`evict_to_caps`) that differs
only in whether a `keep` slot is protected.

## `enforce_caps`

The keep-less cap pass (`evict_to_caps` with no protected slot). Run after a
cache mutation releases its slot lock on a FAILURE path (issue #960). On
success a mutation already ran `evict_lru` under its lock, protecting the
slot it returns; a _failed_ `ensure_clone`/`refetch_clone`/`reclone` returns
before that pass, and a concurrent eviction may have deferred this slot while
its lock was held — leaving the caps exceeded with nothing scheduled to
correct them. `finish_mutation` runs `enforce_caps` once the lock is free (no
slot is protected because a failed mutation has no slot to return), so the
deferred candidate is finally accounted for. It acquires only `EVICTION_LOCK`
and `try_lock`s candidates — never held while a slot lock is held — so it
cannot deadlock with a success-path `evict_lru`. Best-effort: a failure here
is logged, and the mutation's own error is what propagates.

## `ENV_MUTEX`

`scratch_root()` reads process-global env vars; serialize any in-crate
test (in this module or elsewhere, e.g. `recovery_tests.rs`) that touches
it, so the whole `cargo test` binary's parallel test threads never race on
`KHIVE_GIT_DIGEST_SCRATCH_ROOT`/cache-cap env vars/`PATH`. A
`tokio::sync::Mutex` rather than `std::sync::Mutex` so async tests can hold
the guard across `.await` points (`blocking_lock()` for this module's plain
sync `#[test]`s).

## Test module notes

- `ensure_clone_cleans_up_staging_dir_on_clone_failure`: a `git clone`
  failure must not leave a staging wrapper behind under the private
  namespace — `evict_lru` deliberately never touches non-owned names, so a
  leaked staging dir would otherwise accumulate forever across repeated
  failures.
- `stale_staging_sweep_removes_an_abandoned_wrapper_lacking_a_lock_file_once_old`:
  the age-fallback path for a wrapper that crashed before writing its own
  lock file.
- `stale_staging_sweep_preserves_a_wrapper_whose_lock_is_still_held_past_the_age_fence`:
  the blocking-finding acceptance test — a wrapper whose lock a live handle
  still holds must survive the sweep a full year past the old 24h age
  fence, proving liveness (not age) is the deletion criterion.
- `stale_staging_sweep_removes_an_abandoned_wrapper_even_when_fresh`: the
  flip side — an abandoned wrapper (lock file present, nothing holds it) is
  reclaimed even when it was created moments ago.
- `staging_sweep_preserves_foreign_nested_and_nondirectory_entries_even_when_stale`:
  pins the containment/name/type boundary with every fixture entry driven
  stale by a far-future `now`, so only those checks (never freshness) can
  save them — regression coverage for a prior version of this fixture that
  dated preserved entries in the future, so they never reached those checks
  at all.
- `remove_owned_entry_deletes_a_genuinely_owned_slot` /
  `remove_owned_entry_refuses_a_symlink_planted_at_the_cache_key_path_after_the_check`:
  the fd-verified deletion path (`delete_verified_owned_entry`) still
  deletes a genuinely owned slot, and independently refuses a symlink at
  the cache-key path even if some future caller skipped the earlier
  pathname-based `is_owned_entry` gate.
- `dir_size_errors_when_the_root_itself_is_missing` (PR #847): the walk
  root vanishing must surface as an error, never a laundered `Ok(0)` —
  distinct from a descendant vanishing beneath a still-existing root.
- `evict_lru_errors_when_keep_itself_is_missing`: `evict_lru`'s `keep`
  argument — every caller has just touched or freshly installed `keep`
  immediately before calling `evict_lru`, so `keep` vanishing is a real
  problem to surface, not a maybe-absent slot.
- `dir_size_tolerates_a_subdirectory_removed_mid_walk`: issue #842's macOS
  ENOENT flake family. This is a genuine cross-thread filesystem race, not
  a fully deterministic single-shot repro — a `std::sync::Barrier` releases
  both threads at the same instant, a wide fan of sibling subdirectories
  gives the walk many entries to still be processing when the deleter
  runs, and the whole race is repeated 200 times so the window is almost
  certain to be hit at least once.
- `dir_size_errors_when_the_root_is_removed_mid_walk`: companion test
  pinning the other half of the PR #847 contract — when the vanishing path
  is the walk root itself (not a descendant), `dir_size` must surface an
  error. Same barrier-race harness, but `root` is left empty (an
  empty-directory removal is a single `rmdir` syscall, the same order of
  cost as the `symlink_metadata`/`read_dir` calls `dir_size` opens with —
  a populated root, by contrast, has its own directory entry removed
  _last_ by `remove_dir_all` after every child, which would make the
  root-vanish race effectively unreachable). Runs 500 iterations and
  asserts the race was hit at least once.
- `refetch_clone_updates_an_existing_slot_to_the_remote_tip`: the primary
  #765 acceptance path — standing in for genuinely corrupt/incomplete
  objects, which `git fetch --refetch` repairs the same way (re-obtaining a
  complete fresh packfile from the remote).
- `refetch_clone_over_cap_cleanup_never_deletes_an_unproven_slot`:
  remediation (issue #765) — `refetch_clone`'s over-cap cleanup must go
  through the same ownership guard `reclone` uses, not a raw
  `remove_dir_all`, AND must propagate that guard's failure rather than
  discarding it. Since a later fix added a pre-fetch ownership re-check,
  this markerless slot is now refused before `fetch_refetch` even runs
  (see the next test) rather than at the over-cap cleanup step this test
  originally targeted — the assertions still hold, so this remains a
  valid regression guard for the cleanup path once a slot somehow reaches
  it un-owned.
- `refetch_clone_refuses_a_markerless_slot_under_the_cap`: remediation
  (issue #765 follow-up PR #788) — `refetch_clone` must refuse a
  markerless slot _before_ ever calling `fetch_refetch`. The origin is
  given fresh history so a fetch that ran despite the missing marker would
  be directly observable via a moved `HEAD`.
- `reclone_replaces_a_slot_whose_refetch_cannot_succeed`: #765's fallback
  path — a refetch that cannot repair the slot (simulated by pointing the
  existing slot's `origin` remote at a nonexistent path so `git fetch
  --refetch` itself fails) is followed by `reclone`, which ignores the
  broken clone entirely and clones fresh from the still-good
  `canonical_url`.
- `reclone_refuses_to_replace_a_foreign_looking_directory`: ownership
  guard (ADR-088 Amendment 1 / PR #761) — `reclone` must never delete a
  directory that doesn't prove itself an owned cache slot, even though its
  path is exactly where the cache key says the slot should be.
- `ensure_clone_refuses_a_markerless_git_directory_at_the_cache_key_path`:
  remediation (issue #765) — the directory is a genuine Git repository (so
  the pre-fix `repo_dir.join(".git").exists()` check alone would have
  accepted it) but is missing the `.khive-last-used` marker, standing in
  for an operator's own repository landing on the same cache-key path
  under an overridden `KHIVE_GIT_DIGEST_SCRATCH_ROOT`.
- `ensure_clone_refuses_a_symlink_at_the_cache_key_path`: same guard,
  symlink variant — `is_owned_entry` requires the cache-key path itself to
  be a real directory, not a symlink to one.
