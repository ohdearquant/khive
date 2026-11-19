# MCP startup benchmarks

`serve_startup` measures a new helper process on real stdin/stdout, from spawning
that helper through the complete response to its first MCP `tools/call` after
`initialize`. The helper re-executes the benchmark binary, builds the server with
`build_server`, and runs `serve_server`. Criterion runs only in the parent.

| Group | Scenario        | Input                                                                           | Measurement                                                             |
| ----- | --------------- | ------------------------------------------------------------------------------- | ----------------------------------------------------------------------- |
| `mcp` | `serve_startup` | Private, generated 100-entity KG; one process and one `stats()` call per sample | Fixed individual-sample median/p99 report, plus Criterion startup trend |

The parent creates a fresh database with deterministic UUIDs (seed namespace
5655) and names, verifies all 100 entity writes, and joins its database writer
before starting the timer. The database is outside the helper's private home
folder. Each child receives an isolated configuration and private lock directory;
it selects only `kg`, disables embeddings, and uses direct stdio with the events
split disabled. No existing database, daemon, model, or network service is needed.
The Cargo test safety marker is preserved. Temporary directories are private;
Windows system-directory variables needed for process loading are retained.
Each Tokio executor uses at most two blocking threads.

The measured interval includes process creation, helper runtime creation,
database opening, production server and registry construction, the stdio
initialize exchange, and one `tools/call` carrying `stats()`. Parent-side pipe
registration and protocol work inside that interval are included. This is a
fresh-process measurement with a recently written fixture; it does not claim
cold OS caches or measure daemon forwarding.

Each sample captures the response arrival time before decoding its final JSON.
Outside that interval, it checks the successful response and operation receipt,
exactly 100 live entities, zero edges and notes, and the caller-visible/live-only
count scope. The parent closes stdin, requires exit status zero, and reaps the
helper before admitting the sample or starting another one. These checks and
fixture deletion are untimed. Errors fail the benchmark, including failures to
initialize, wrong fixture counts, malformed responses, and unsuccessful shutdown.

Protocol reads have a 30-second deadline, a 1 MiB frame cap, and at most 16 frames
per expected response. After stdin closes, the helper has a separate 10-second
exit deadline. A child guard kills and waits on failure or unwinding; stderr is
inherited rather than left in an undrained pipe. These are harness failure bounds,
not startup performance promises or guarantees against uninterruptible OS I/O.

A fixed report always takes 101 independent process samples. The median is sorted
rank 51; p99 uses nearest rank `ceil(0.99 * 101) = 100`. It prints both values in
nanoseconds. The Criterion companion uses `iter_custom` and sums only the same
startup intervals, with each fixture and process handled sequentially. Its
aggregate estimate is a trend measurement, not the individual-sample p99. There
is no allocation or RSS row because a process boundary does not expose an
allocation-per-request measurement.

## Performance guarantees

| Operation              | Input                           | Metric         | Bound                                        |
| ---------------------- | ------------------------------- | -------------- | -------------------------------------------- |
| `serve_server` startup | stdio, 100-entity KG, one round | median latency | Advisory trend against the recorded baseline |
| `serve_server` startup | stdio, 100-entity KG, one round | p99 latency    | Advisory trend against the recorded baseline |

Both rows are advisory timing trends against rolling and fixed references. No
fixed latency percentage or deterministic allocation/count gate is claimed by
this table. Fixture and lifecycle assertions validate what was measured.

## Baseline

| Scenario        | Samples | Median  | p99     | Commit  | Toolchain | Machine | Producer                                          |
| --------------- | ------- | ------- | ------- | ------- | --------- | ------- | ------------------------------------------------- |
| `serve_startup` | 101     | pending | pending | pending | pending   | pending | Owning bench-track component / qualified-host run |

This is the first startup baseline; values remain pending until the suite is
measured. There are no previous startup timing values to preserve. The daemon
lifecycle documentation's installed-file check frequency remains a separate
frequency contract, not a latency baseline.

## Execution

From `crates/`:

```sh
cargo bench -p khive-mcp --benches -- --quick --noplot
```

The fixed 101-sample report also runs in quick mode; every reported sample creates
and reaps its own helper. Run on a qualified host with ordinary filesystem and
process resources available. Record the exact commit, toolchain, machine, fixed
median/p99 report, and Criterion artifacts when filling in the baseline.

The `CoordinatorService::locate` implementation benchmark belongs to `kkernel`
(issue #5665). The private `hash_embed` helper is outside this target's API and
scope. Workflow selection is maintained separately from this crate's benchmark.
