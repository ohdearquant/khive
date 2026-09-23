# ADR-194: Bounded SQLite WAL Extent Ceiling Under a Pinned Reader

**Status**: accepted (2026-09-23)\
**Date**: 2026-09-23\
**Authors**: khive maintainers\
**Tracking**: Refs #1876\
**Implementation**: none in this ADR; accepting or merging this document does not close #1876

## Context

ADR-154's disk-reserve admission floor stops a new cooperating write from starting when sampled
free space is at or below a configured reserve. It cannot bound the WAL of a transaction already
admitted, and it explicitly does not fence other processes or reserve bytes for recovery. Neither
checkpoint scheduling nor reader cancellation can establish a bound while a reader holds a WAL
snapshot indefinitely: khive's scheduled checkpoint task backfills on a PASSIVE pass and only
attempts a TRUNCATE escalation once accumulated log frames cross a fixed threshold, and that
escalation can itself block behind the same pinned reader for a bounded interval before giving up.
#1876 asks for a different, harder guarantee: a configured ceiling on the WAL's active extent that
holds under continuous multi-client writes against a real pinned snapshot, with a typed, retryable
refusal in place of the 3.8x overshoot the issue measured at `wal_autocheckpoint=4000`.

Physical WAL file length, active WAL extent, and checkpoint debt describe different quantities.
SQLite can reuse an already-allocated WAL region without shrinking the file on disk, so file length
alone is not a safe admission signal in either direction. Checkpoint debt (the gap between
committed and backfilled frames) can read zero while a reader still holds an earlier read mark and
blocks WAL reset; zero debt is therefore not proof that a reset can happen. ([Write-ahead logging](https://sqlite.org/wal.html); [Checkpoint API](https://sqlite.org/c3ref/wal_checkpoint_v2.html))

## Decision

### 1. Scope and guarantee

When `wal_ceiling_bytes = C > 0` for a backend, each participating writer enforces a per-database
maximum WAL extent of `C` bytes, covering the WAL header, frame headers, uncommitted spill, and
commit-time writes. No WAL write whose checked end offset would exceed `C` is forwarded to the
underlying I/O layer, including writes SQLite performs while completing `COMMIT`. Other routes that
extend or preallocate the WAL file obey the same limit.

Let `P0` be the WAL file's physical length when this policy activates. With every participating
writer enforcing the same `C`, physical WAL length remains at most `max(P0, C)`; if `P0 <= C`,
length remains at most `C`. A retained tail above `C` at activation is reported, not destructively
truncated, and is not license for new writes above `C`; it is recovered through the ordinary
checkpoint path described in §4.

The guarantee survives an indefinitely pinned read-only connection anywhere: this process, another
khive process, an older khive binary, or a non-khive reader. It does not survive an unguarded or
differently configured writer, and it does not bound database growth, temporary files, allocated
filesystem blocks, or consumption by other applications on the volume. ADR-154's free-space
admission floor remains the separate policy for those cases.

### 2. Metric and enforcement

Diagnostics expose committed frames `L` and backfilled frames `K` from a valid same-generation
observation; checkpoint debt is `L - K`. Debt is a backfill signal, not a per-holder read mark, and
must never be read as proof that a reset can occur. For page size `p`, the WAL file format fixes a
32-byte header and a 24-byte header per frame, so the ordinary committed-frame extent is
`32 + L*(p+24)` bytes. ([WAL file format](https://sqlite.org/fileformat2.html#walformat)) That arithmetic is a diagnostic estimate: it
excludes uncommitted spill and commit-time padding and is not itself the enforcement mechanism.

Enforcement is a WAL-aware I/O limiter installed ahead of the underlying SQLite I/O implementation,
guarding every write, truncate, allocation, and file-control call the WAL can grow through. The
preferred implementation is a VFS wrapper over SQLite's public I/O interface. ([I/O methods](https://sqlite.org/c3ref/io_methods.html)) The implementation must demonstrate complete
interception on every supported platform and I/O mode; an enabled ceiling on an unsupported
configuration is a fail-closed configuration error, not silent best-effort coverage.

A fresh execution-time check refuses a new write unit when the projected next end offset would
exceed `C`. It must not rely on a stale cached observation, and observation failure or a
generation mismatch is a typed unavailable result, never a coerced zero. The limiter stays active
through the whole write unit, including `COMMIT`, because no universal pre-admission budget for a
transaction's WAL growth exists: an admission-time-only check cannot bound growth that happens
during the body or at commit. A WAL commit hook is not a substitute enforcement point: SQLite
invokes it after the commit and its associated write lock have already been released, so a hook
error cannot undo a completed commit. ([WAL commit hook](https://sqlite.org/c3ref/wal_hook.html))

### 3. Write outcomes and recovery

Two structured stages register under ADR-135 F6, alongside ADR-154's three disk-floor stages:

| Stage                             | Meaning                                                                                                                                                                                                                                                                                                                                                                                       | Retry contract                                                                                                                                                                                                       |
| --------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `sqlite_wal_capacity_refused`     | A write unit was refused before it ran, or a proven guard-origin capacity failure settled with the whole unit rolled back; carries `phase=admission\|execution`, `C`, database identity, and the proven outcome; carries `fits_after_reset=false` only when the refused unit began at the start of an empty WAL, so the refusal itself proves that no reset or reader release can make it fit | Retry the same unit only after WAL reuse, a policy increase, or an application-approved reduction in unit size; never automatic. With `fits_after_reset=false`, only a policy increase or a smaller unit can succeed |
| `sqlite_wal_capacity_unavailable` | The enabled limiter could not establish safe admission or could not install/maintain its enforcement capability                                                                                                                                                                                                                                                                               | No automatic retry; the failure names the capability or probe that needs repair                                                                                                                                      |

These map onto ADR-135 F6's existing outcome vocabulary as `rejected-before-admission` (refused at
admission) and `failed-before-commit` (a guard-origin refusal mid-unit). A transaction that hits
the limiter mid-unit becomes failed and reaches whole-transaction rollback; the limiter must not let
calling code observe a partial, uncapped commit. The typed capacity result is returned only after
rollback and connection state are proven, never merely attempted. An already-completed commit is
never relabeled as a capacity refusal, and native SQLite primary/extended codes (including a real
`SQLITE_FULL` the limiter did not cause) are preserved and distinguished from a limiter-origin
refusal.
Because a VFS wrapper can only return an SQLite result code, the limiter records each refusal it
causes in a connection-scoped last-refusal record (the write unit and the refused end offset), and
the write path reads and clears that record after the error returns: an error with a matching
record is a limiter-origin refusal, and one without is a native SQLite error.

**Recovery the capacity path itself triggers.** khive's checkpoint task already runs a bounded
TRUNCATE escalation, `maybe_truncate`, on its own dedicated connection, gated by a frame-count
threshold (`truncate_high_water_pages`, default 20,000) and a minimum interval
(`truncate_min_interval`, default 300s) between attempts, each attempt itself bounded by
`truncate_busy_timeout` (default 2,000ms). At a configured `C` well below the bytes that 20,000
frames represent, `L` never reaches that threshold, because the limiter refuses new writes before
`L` can grow that far: relying on the existing schedule alone would leave writes refused with no
route back to admission.

This ADR closes that gap at the point of refusal, not by waiting on the periodic schedule. A
capacity refusal signals the same dedicated checkpoint connection to attempt an immediate TRUNCATE
on its next tick, with the frame-count threshold bypassed for that signaled attempt. The minimum
interval between attempts and the per-attempt busy-timeout bound stay in force, tracked separately
from the periodic schedule's own timer so a ceiling-triggered attempt does not starve, or get
starved by, the unrelated periodic one; the interval defaults to the same bound as one attempt's own
worst-case cost (`truncate_busy_timeout`), so a burst of refusals cannot request the writer lock
more often than that. A successful attempt frees WAL extent for the caller's next retry. An
unsuccessful attempt (a pinned reader, a busy writer, or a disabled checkpointer) leaves the typed
refusal in force; a still-pinned reader is the one case this ADR does not promise to resolve, stated
explicitly in §1.

Reader release, cleanup, and cancellation policy remain governed by ADR-005, ADR-091, and the
separately specified work in #1846; this ADR does not evict readers and does not infer revocation
authority from a holder's age or process ID. Existing checkpoint policy is otherwise unchanged: this
ADR adds no automatic stronger checkpoint beyond the signaled attempt above, and does not force
truncation to make physical length track checkpoint debt.

**Relationship to ADR-154's terminator-guard rejection.** ADR-154 rejects probing at `COMMIT` or
`ROLLBACK` because a floor re-checked there could refuse admission after a transaction body has
already run, leaving its outcome unresolved. This ADR's limiter does the opposite: it never blocks
a terminator from executing, it only refuses the specific over-cap WAL write a terminator would
otherwise perform and always completes that terminator through a proven whole-transaction rollback
before returning a result, so no caller ever observes an unresolved outcome: the exact property
ADR-154's rejection protects. ADR-154 §5 is amended accordingly; see the companion edit below.

Retry scope is one real transaction or atomic unit: a multi-commit script or request preserves
earlier committed units and marks only the failed unit retryable. A unit that cannot fit under `C`
even against an empty WAL needs a larger `C` or an application-level change; reader release alone
cannot fix it. The refusal says so (`fits_after_reset=false`) whenever it can prove it, so a caller
is never left retrying a unit that no reset can admit.

### 4. Reset-feasible configuration floor

A `C` too small to ever hold one committed frame can never admit a write, even immediately after a
successful reset, independent of the recovery path in §3. This ADR closes that case at
configuration load rather than at every subsequent refusal.

For a backend's configured page size `p`, the reset-feasible minimum is:

```
reset_feasible_minimum_bytes(p) = WAL_HDRSIZE + (p + WAL_FRAME_HDRSIZE)
                                 = 32 + (p + 24)
                                 = p + 56
```

using the WAL header size (32 bytes) and per-frame header size (24 bytes) fixed by the WAL file
format. ([WAL file format](https://sqlite.org/fileformat2.html#walformat)) `p` is read from the
backend's own configured page size at load time, never assumed to be a fixed value. Configuration
load rejects a nonzero `wal_ceiling_bytes` below `reset_feasible_minimum_bytes(p)` for that backend
as a configuration error; it never clamps the value up or silently disables the ceiling.

This is a necessary, not a sufficient, condition: a `C` above the minimum admits at least one frame
after a reset, but ordinary workloads need meaningfully more headroom than the bare minimum to make
useful progress. Choosing `C` for a real workload is a separate, workload-informed decision; this
floor only refuses a configuration that could never make forward progress at all.

### 5. Configuration and compatibility

Resolve `wal_ceiling_bytes` per writable SQLite backend from its `[[backends]]` field, then
`KHIVE_SQLITE_WAL_CEILING_BYTES`, then **0 (disabled)**. A nonzero value must fit supported offset
arithmetic (checked, not wrapping) and satisfy §4's reset-feasible minimum. Invalid, overflowing,
unsupported, or below-minimum values are configuration errors, never fallback or silent clamping.
Nonzero configuration on a memory or non-WAL backend is rejected; a read-only backend enforces no
writer policy.

Zero disables the WAL ceiling independently of `disk_reserve_bytes`; the two knobs are unrelated and
one being zero does not affect the other. Startup and diagnostics explicitly report disabled state.
This ADR promises a bound only for an explicitly enabled, nonzero `C`; it does not establish a
positive out-of-the-box default. Choosing a positive default is a separate, workload-evidenced
change.

Include the effective numeric ceiling, including zero, in ADR-096's `config_id` for the implicit
main backend and every named backend, in the same deterministic backend order the existing topology
fold already uses; this follows the same fingerprinting ADR-154 already applies to its reserve and
deadline. Equal values from different configuration sources fingerprint identically; a changed
value rejects daemon reuse. Runtime frame counts, free space, and file-identity observations never
enter the fingerprint.

Policy changes require a coordinated drain/restart of participating writers; there is no in-flight
hot shrink. Lowering `C` below the current active extent refuses new work until safe recovery; a
reusable retained tail need not block writes that fit within `C`. Disabling the ceiling is an
explicit, visible loss of protection. Equal client/daemon configuration is not proof that external,
non-khive writers participate in the same limit.

### 6. Coverage and observability

The limiter covers every participating WAL writer: queued and pooled writers, standalone/manual and
autocommit execution, queue-off compatibility routes, bootstrap, migrations, maintenance, and
incidental audit/telemetry writes. Connection reopen and recovery preserve the policy. File aliases
resolve to one consistent policy. An attempt to switch to an unguarded VFS, change journal mode, or
attach an uncovered database fails rather than silently bypassing the ceiling.

Expose per database: configured ceiling and its source, enabled/capability state, page size,
physical WAL length, committed and checkpointed frame counts, pending frames, observation
timestamp/validity, and refusal/rollback outcomes. An unknown observation is distinct from zero.
Guard reporting must not itself require a successful write to the guarded WAL.

### 7. Acceptance

Fixtures are temporary databases with strict byte ceilings, injected observations, barriers, and an
instrumented I/O boundary. No test pressures a real host checkout, home, or runner filesystem; a
real-exhaustion arm uses only ADR-154's verified isolated device and is skipped, explicitly, where
isolation cannot be proved.

| Witness                    | Positive case and required negative control                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| -------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Real pin, multi-client     | `BEGIN` plus a real table read on a separate reader connection, held behind a release barrier. Drive fixed-size writes from at least two OS processes and several clients against a small fixture `C` (for example 1 MiB). Observe every delegated WAL write/extension, not only final file length: no accepted end offset above `C`, no transient overshoot. A disabled-limiter control must exceed `C` within the same bounded work budget.                                                                                                                                                                                                                                                                                            |
| Large transaction          | Admit below `C`, dirty past remaining capacity with forced cache spill and indexed/triggered work, cross the cap during `COMMIT`. Assert exact whole-transaction rollback, no partial rows, no out-of-cap delegated I/O, the typed capacity stage, and successful reuse afterward. An admission-only mutant (checks at `BEGIN`, never during the body or `COMMIT`) must fail this test.                                                                                                                                                                                                                                                                                                                                                  |
| Ceiling-triggered recovery | Fixture `C` sized for roughly 50 frames, well under `truncate_high_water_pages`, with no pinned reader. Drive writes to the first capacity refusal; assert the refusal signals a bounded TRUNCATE attempt on the dedicated checkpoint connection (observed via an injected hook, not by waiting on the unrelated periodic scheduler), that the attempt completes within `truncate_busy_timeout`, and that a retry after the attempt is admitted, all without waiting for the frame-count/minimum-interval schedule that gates the periodic tick. A companion arm pins a real reader first: the signaled attempt must bound-block up to `truncate_busy_timeout`, then fail, leaving the typed refusal in force with no hang and no crash. |
| Reset-feasible floor       | Load configuration with `wal_ceiling_bytes` one byte below `reset_feasible_minimum_bytes(p)` for the fixture's page size: load must fail with a typed configuration error, never clamp or silently disable. A boundary-equal value (`wal_ceiling_bytes == reset_feasible_minimum_bytes(p)`) must load successfully and admit exactly one minimal frame before the next write is refused.                                                                                                                                                                                                                                                                                                                                                 |
| Debt versus extent         | Pin a latest WAL snapshot and backfill to `K = L`: show zero debt does not authorize assuming a reset occurred. Separately release all readers and invoke recovery, including a `C` below `truncate_high_water_pages`; writes resume even with an unchanged, larger retained file. A debt-only gate and a physical-length-only gate each fail their respective control.                                                                                                                                                                                                                                                                                                                                                                  |
| External reader / writer   | A raw, read-only SQLite process pins the snapshot: the bound still holds. A deliberately unguarded writer is a bounded negative control demonstrating the stated writer-participation limit, not a claim of global fencing.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| Starting above cap         | Create a bounded historical tail above `C`, then enable the policy. Active excess is distinguished from reusable retained allocation: no destructive truncation, no extension beyond `max(P0, C)`. Safe reset restores capped writes.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| Failure and cancellation   | Inject preflight unavailability, checked-add overflow, a real native `SQLITE_FULL`/I/O failure, a guard-origin refusal, rollback failure, and crash/reopen. Prove cause attribution (never relabeling a real `SQLITE_FULL` as a guard refusal), worker settlement per the ADR-005 amendment, and known-versus-unknown outcome.                                                                                                                                                                                                                                                                                                                                                                                                           |
| Coverage                   | Enumerate every writer/growth route and supported VFS/platform; deliberately bypass one route to prove its own test fails. Cover file-control allocation hints, truncate extension, rollback/savepoints, WAL reset, aliases, and attempts to change journal/VFS configuration underneath an enabled ceiling.                                                                                                                                                                                                                                                                                                                                                                                                                             |
| Diagnostics                | Show configured `C`, enabled state, `p`, `L`, `K`, debt, physical bytes, observation validity/age, and refusal causes. An unavailable/error observation is unknown, never a healthy zero. Equal effective policies fingerprint equally; a changed value rejects daemon reuse.                                                                                                                                                                                                                                                                                                                                                                                                                                                            |

## Alternatives considered

- **Disk floor only (ADR-154 unmodified).** Useful admission hygiene for sampled headroom, but
  leaves WAL extent and in-flight growth unrestricted; it does not answer #1876.
- **Admission threshold only, no I/O limiter.** Permits unbounded overshoot during the body or at
  `COMMIT`. Valid only alongside a proven, finite, conservative per-unit WAL budget; no such budget
  is established for khive's write surface.
- **Checkpoint debt or physical-length predicate only.** Debt confuses backfill with reset
  eligibility; physical length confuses retained allocation with active pressure. Neither bounds the
  next write.
- **Reader eviction as the safety mechanism.** May improve recovery latency where khive owns the
  reader, but cannot reach an external process and cannot guarantee prompt release; kept as a
  separate, non-safety-critical improvement (§3).
- **Rely on the existing scheduled TRUNCATE threshold alone, with no ceiling-triggered attempt.**
  Rejected: for any `C` below the bytes 20,000 frames represent, the schedule's own frame-count gate
  never arms, because the ceiling itself keeps `L` below that threshold. This is the gap §3 closes.
- **Allow a below-minimum `C` with a runtime-only refusal.** Rejected: a `C` under
  `reset_feasible_minimum_bytes(p)` cannot admit a single frame even immediately after a clean
  reset, so the runtime symptom is indistinguishable from a permanently misconfigured database;
  refusing at load surfaces the mistake once, at the moment it is made.
- **Post-commit hook or periodic interruption as the enforcement point.** Too late (commit already
  released the write lock) or too coarse for a hard extent bound, and unsafe as a retry signal.
- **A positive ceiling by default.** Deferred until representative transaction and maintenance
  workload evidence and an acceptable refusal rate are established.

## Consequences

An enabled deployment exchanges unrestricted transaction size and write availability for a precise,
per-database WAL extent bound and forward progress that no longer depends on the periodic
checkpoint schedule reaching its own frame threshold. An external reader can still cause a
prolonged, typed refusal; that is the guarantee's stated writer-only scope, not a defect. The I/O
limiter expands the surface that must be proven correct for storage integrity, because a limiter bug
that misjudges an offset risks the WAL itself. A configuration below the reset-feasible minimum is
refused before it can ever run, rather than surfacing as an unexplained permanent refusal in
production. External writers and unrelated filesystem consumption remain outside this guarantee, as
they do for ADR-154's floor.

## References

- #1876: an actual WAL ceiling under a pinned reader
- #1844: SQLite/WAL disk-reserve pre-write guard
- #1846: maximum read-transaction age across connection types
- #1417: physical WAL retention
- [ADR-005](ADR-005-storage-capability-traits.md): storage capability traits and write completion
- [ADR-091](ADR-091-wal-snapshot-lifetime.md): checkpoint and WAL-pin governance
- [ADR-096](ADR-096-warm-daemon-per-request-identity.md): `config_id` coherence fingerprint
- [ADR-135](ADR-135-write-scaling-demand-before-ownership.md): writer error taxonomy (F6)
- [ADR-154](ADR-154-sqlite-disk-reserve-admission.md): disk-reserve admission floor
