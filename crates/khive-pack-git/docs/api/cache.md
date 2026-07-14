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
Eviction only ever removes entries it can *prove* it owns (`is_owned_entry`:
a 16-hex cache-key directory name containing both a `.git` dir and the
`.khive-last-used` marker) — a `KHIVE_GIT_DIGEST_SCRATCH_ROOT` override
pointed at a broader or pre-existing directory must never lose unrelated
operator data.

A per-clone size cap (`digest_cache_clone_max_bytes`) rejects a clone/fetch
that grows past its own budget *before* it ever enters the addressable cache
slot: `ensure_clone` clones/fetches into a staging directory outside the
cache root, measures it, and only moves it into `<root>/<cache_key>/` when
it is under the cap. A too-large clone is deleted from staging and never
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
`.khive-last-used` there, and only *moved* into the addressable
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
so this function cannot rely on the caller having checked recently. There
is no same-key serialization for cache mutation in this crate today (a
concurrent `ensure_clone`/`reclone` racing this same slot is not otherwise
excluded) — this re-check narrows the adoption bug but does not close a
true concurrent-writer race.

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

## `install_fresh_clone`

Shared staging-clone-then-move path for both a first-time `ensure_clone`
and a `reclone` repair: clones into a private staging directory outside the
cache root, measures it against the per-clone cap, writes the
`.khive-last-used` ownership marker into the staging directory itself, and
only then moves it into the addressable `<root>/<cache_key>/` slot — an
oversized clone never enters the cache slot, and because the marker is
written before the atomic rename, a process interruption between clone and
rename can never leave a live, markerless slot at the cache-key path (issue
#765).

## `remove_owned_entry`

Removes `repo_dir` only when it is a direct child of `root` AND passes
`is_owned_entry` — refuses (`CacheError::UnsafeToReplace`) rather than
deleting anything else, including a not-yet-existing or foreign-shaped
path. A slot that does not currently exist is not an error: there is
simply nothing to remove before installing a fresh clone.

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
maintenance runs as a *detached background child*
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

Tolerant of a *descendant* disappearing mid-walk (a vanished entry beneath
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
the caller asked to keep is actually gone. A listed *candidate* entry is
different — another `evict_lru`/`ensure_clone` repairing the same root can
legitimately delete it between the `read_dir` listing and the `dir_size`
call, so that vanish is tolerated by skipping the entry rather than
aborting the whole pass.

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
  failure must not leave a `.staging-<uuid>` directory behind — `evict_lru`
  deliberately never touches non-owned names, so a leaked staging dir
  would otherwise accumulate forever across repeated failures.
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
  *last* by `remove_dir_all` after every child, which would make the
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
  markerless slot *before* ever calling `fetch_refetch`. The origin is
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
