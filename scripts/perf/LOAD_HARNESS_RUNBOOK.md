# Load-harness driver — runbook

`bench_load_harness.py` drives many concurrent front-end connections across
many tenant namespaces against one warm daemon, and reports what it observed
across four channels. It does **not** assert pass/fail on the underlying gate
dimensions — it measures and reports. Read the JSON report yourself.

## Modes

- `--mode real` (default): uses the installed/production `kkernel` binary
  (real embedder, GPU-backed on macOS via Metal). Acquires the machine-wide
  Metal-GPU serialization lock (bounded 30 min wait, fails loud on timeout)
  before spawning anything. Required for latency/cold-start observations —
  a hash-based bench embedder has no cold-init cost to spike.
- `--mode bench`: uses a `kkernel-bench` binary built with the
  `bench-embedder` cargo feature (deterministic hash embedder, no GPU, no
  lock). Build it first:

  ```bash
  cd crates && cargo build --release -p kkernel --features bench-embedder
  cp crates/target/release/kkernel crates/target/release/kkernel-bench
  ```

  Bench mode is the fast local loop for the concurrency/WAL/backpressure/
  attribution/fallback dimensions; it cannot reproduce the embedder cold-start
  spike.

## Running

Reduced local smoke (proves the plumbing runs without erroring, at 1/5 scale).
Relies on the 7-pack `--packs` default: the hermetic single-file scratch DB
omits `session`, whose lazily-applied mirror schema fails bootstrap recall on
that config and whose background writes would confound the reduced-scale gauges.

```bash
uv run scripts/perf/bench_load_harness.py --mode real --workers 20 --tenants 4 --ops-per-worker 20
```

Full acceptance shape (100 connections × 20 tenant namespaces, the actual
gate target). Pass `--packs` explicitly to measure the full 11-pack production
posture, including `session`, `git`, `code`, and `workspace`, against a real multi-pack
config:

```bash
uv run scripts/perf/bench_load_harness.py --mode real --workers 100 --tenants 20 --ops-per-worker 50 \
  --packs kg,gtd,memory,brain,comm,schedule,knowledge,session,git,code,workspace
```

`--workers` must be an exact multiple of `--tenants` (workers are split
evenly, 5/tenant at the default 100×20 shape). Every worker fires a weighted
mix of `memory.recall` / `knowledge.search` / `knowledge.compose` (reads) and
`memory.remember` / `create` (writes) against its own persistent front-end
process; one worker per tenant additionally runs a `comm.send` + `get`
readback pair to check write attribution.

The harness always runs with `KHIVE_DAEMON_STRICT=1` and
`KHIVE_WRITE_QUEUE=1` — the run posture required for the load/perf acceptance
run. It uses a fresh scratch DB under a temp directory; the live
`~/.khive/khive.db` is never touched, and the scratch directory + daemon are
torn down on exit unless `--keep` is passed.

## Reading the report

The report prints as JSON to stdout (and optionally to `--report PATH`).
Top level:

- `smoke_result`: `"PASS"` or `"FAIL"` — purely whether the concurrency
  plumbing itself completed without a worker crashing or hanging past
  `--worker-timeout`. This is **not** a verdict on any of the nine gate
  dimensions.
- `smoke_errors`: any worker crash/hang or driver-level exception.
- `oracle_probe_t0` / `oracle_probe_post_load`: the daemon-frame snapshot
  probe result at the start and end of the run (see below).
- `dimensions`: one entry per gate dimension, each carrying its own
  observation channel, the raw numbers collected, and a plain-language note.
  None of these entries assert a threshold — they report what was measured
  this run so a human (or a later, explicitly-gating harness pass) can judge
  it against the calibrated targets.

### The oracle (daemon-frame) channel is probe-gated

Dimensions 4, 5, and part of 7 read daemon-side gauges (WAL pages, oldest
open-transaction age, write-queue depth) through a frame field the wire
protocol does not expose yet. The harness speaks the daemon's Unix socket
directly and sends that field as a probe:

- If the daemon's response grows a populated `metrics` key, the oracle is
  `"LIVE"` and the raw gauge values are included in the report.
- If the field is silently ignored (today, on any daemon that predates the
  metrics-frame change), the oracle is `"PENDING"` and those three dimensions
  report `"PENDING"` instead of a number. The harness never errors or hangs
  waiting for this — it degrades cleanly and keeps running every
  frame-independent dimension normally.

No code change is needed here once the metrics frame lands upstream — the
probe will simply start seeing the new key.

## Known gaps this round

- Dimension 3 (embedder cold-start) has no confirmed log-event text to grep
  for yet; it reports `"not-implemented-this-round"` rather than a fabricated
  signal. Dimension 2's latency shape is the only indirect corroboration
  available today.
- The nine-dimension thresholds (WAL ceiling, p99 targets, etc.) are not
  encoded as pass/fail gates in this driver — by design, the first
  full-scale run calibrates them; a later pass can add explicit thresholds
  once numbers exist to calibrate against.

## A real finding: per-session actor pinning does not do what it looks like

Setting a per-worker `KHIVE_ACTOR` environment variable alongside an explicit
`--namespace <tenant>` flag does **not** give that worker a distinct
attributed-actor string from its namespace. In this worktree, an explicit
non-local `--namespace`/`--actor` CLI value fills `actor_id` from that same
namespace string before the `KHIVE_ACTOR` env fallback is ever consulted
(the tier-1/tier-3 precedence resolution in the `kkernel mcp` front-end).
Empirically, a worker started with `--namespace tenant_3` and
`KHIVE_ACTOR=tenant_3_actor` in its environment gets its writes stamped with
actor `tenant_3` — not `tenant_3_actor`.

This harness does not assume a specific actor-string convention because of
that. Dimension 8 instead checks two things that hold regardless of the
convention: every write's attributed actor is what the write call itself
echoed back (write-then-read consistency), and distinct tenants receive
distinct attributed actors (no cross-tenant collapse). Both held at the
reduced-scale smoke scale tested here. Anyone building the actor-pinning leg
of the full acceptance run should be aware of this mechanism before assuming
the `KHIVE_ACTOR`-per-session approach yields an independently chosen actor
label.
