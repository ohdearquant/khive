#!/usr/bin/env python3
"""Run the F7 writer sweep: one load-harness run per point, each against its own scratch daemon.

The run order is fixed before the first run and written to plan.json in the output directory:

  1. A/A: the first process count twice, back to back, attempt log on. The difference between the two bounds the
     noise of the harness itself at that count.
  2. Sweep: every process count once, attempt log on.
  3. Paired arm at --paired-workers: --paired-rounds rounds of one run with the attempt log off and one with it on,
     adjacent in time. The order flips each round (off-on, on-off, ...) so drift does not land on one arm. Client
     latency is pooled per arm over the rounds, and the cost line states on minus off at p50 and p99 per op class.
  4. Burst points, when --burst-sizes is given: one run per size at --paired-workers, attempt log on, with that many
     extra arrivals at one instant every --burst-every seconds on top of the steady rate. Each run states how many
     bursts missed the --burst-deadline-ms drain deadline (late, or any arrival refused, failed or unfinished) and how
     many drained late (completed arrivals past the deadline, or an arrival that was not refused never completing,
     whatever the refusals). The knee line names the smallest size with a burst that drained late, and names apart
     the sizes whose bursts missed only through refused arrivals. Bursts add arrivals to a run of fixed length, so a
     burst run offers fewer seconds than a sweep run at the same rate.

Each run gets its own directory under --out with the harness report (harness.json), the harness log and the sink
directory the daemon wrote (sink/), which is kept. Every attempt-log run prints the attempts expected, rows written,
sequence gaps, drop counter and loss bound, then its point verdict (writer_attempt_report.py). A run whose loss bound
is over --loss-limit is VOID and runs again, up to --void-retries more times; the void runs stay on the record. In the
paired arm the whole round runs again, off and on in the same order, so the two stay adjacent. A round still VOID
after its retries leaves both arms. For rounds with failed or outstanding arrivals, the sweep reports completed
client latency separately for both arms. The cost line pools clean rounds. A run with the attempt log off must
leave no writer_attempt row in its sink: one there means the variable reached a daemon it should not have, and
the paired arm is BROKEN.

Every run, both paired arms included, shortens the sink heartbeat and waits for it to settle before teardown
(bench_load_harness.py SINK_HEARTBEAT_MS and SINK_SETTLE_S), which a default daemon does not do. The closing lines
state the heartbeat and settle the runs recorded.

The script reports; it never asserts an envelope. Exit 0 when every point has a reading, 1 when a point is still
VOID after its retries or no paired round was clean enough to pool, 2 when an input could not support a reading
(BROKEN). A line before the cost line says how many paired rounds left both arms for failed or outstanding arrivals.
"""

from __future__ import annotations

import argparse
import json
import math
import pathlib
import subprocess
import sys

sys.path.insert(0, str(pathlib.Path(__file__).parent))
import writer_attempt_report as war  # noqa: E402

HARNESS = pathlib.Path(__file__).parent / "bench_load_harness.py"


def plan_runs(workers, *, aa, paired_workers, paired_rounds, burst_sizes=()):
    """The ordered list of runs: A/A pair at the first count, the sweep, the alternating paired arm, the bursts."""
    if not workers:
        raise ValueError("at least one process count is needed")
    runs = []
    if aa:
        runs += [{"name": "aa-1", "workers": workers[0], "attempt_log": True, "role": "aa"},
                 {"name": "aa-2", "workers": workers[0], "attempt_log": True, "role": "aa"}]
    runs += [{"name": f"sweep-{w}", "workers": w, "attempt_log": True, "role": "sweep"} for w in workers]
    for r in range(paired_rounds):
        order = (False, True) if r % 2 == 0 else (True, False)
        for log_on in order:
            runs.append({"name": f"pair-{r + 1}-{'on' if log_on else 'off'}", "workers": paired_workers,
                         "attempt_log": log_on, "role": "pair", "round": r + 1})
    runs += [{"name": f"burst-{b}", "workers": paired_workers, "attempt_log": True, "role": "burst", "burst_size": b}
             for b in burst_sizes]
    return runs


def harness_argv(run, run_dir, args, python=sys.executable):
    workers = run["workers"]
    if workers % args.workers_per_tenant:
        raise ValueError(f"{workers} processes do not split into tenants of {args.workers_per_tenant}")
    ops_per_worker = max(1, math.ceil(args.rate_per_worker * args.seconds))
    argv = [python, str(HARNESS), "--mode", args.mode, "--workers", str(workers),
            "--tenants", str(workers // args.workers_per_tenant), "--ops-per-worker", str(ops_per_worker),
            "--arrival-rate", repr(workers * args.rate_per_worker), "--mix", args.mix,
            "--worker-timeout", repr(args.worker_timeout),
            "--sink-dir", str(run_dir / "sink"), "--report", str(run_dir / "harness.json")]
    if args.packs:
        argv += ["--packs", args.packs]
    if run["attempt_log"]:
        argv.append("--writer-attempt-log")
    if run.get("burst_size"):
        argv += ["--burst-size", str(run["burst_size"]), "--burst-every", repr(args.burst_every)]
    return argv


def read_sink(sink_dir):
    files = sorted(pathlib.Path(sink_dir).glob("writer_timeouts.*.ndjson"))
    return {f.name: f.read_text().splitlines() for f in files}


def attempt_rows(sink):
    return sum(len(segment["attempts"]) for lines in sink.values() for segment in war.parse_sink_lines(lines))


# bench_load_harness.DRAIN_TIMEOUT_CRASH; a test compares the two.
DRAIN_TIMEOUT_CRASH = "open-loop drain exceeded --worker-timeout"


def drain_timeout_only(harness, harness_rc):
    """True when the harness failed only because arrivals were outstanding at its drain timeout.

    A drain-timeout-only report has outstanding arrivals and no separate worker or daemon failure."""
    errors = harness.get("smoke_errors") or []
    outstanding = (harness.get("arrival") or {}).get("counts", {}).get("outstanding")
    return (bool(outstanding) and harness_rc in (0, 1) and bool(errors)
            and all(str(error).endswith(f": {DRAIN_TIMEOUT_CRASH}") for error in errors))


def read_run(run, run_dir, *, loss_limit, util_limit, queue_p99_limit_micros, harness_rc, burst_deadline_s=None):
    """One run's reading: the point report for a log-on run, client latency and a row count for a log-off run.

    A harness that failed for any reason but arrivals left outstanding at its drain timeout supports no reading,
    including a run where that timeout came with another error. A drain timeout alone is a reading: the point says
    how many arrivals never finished."""
    try:
        harness = json.loads((run_dir / "harness.json").read_text())
    except (OSError, ValueError) as exc:
        raise war.ReportError(f"{run['name']}: the harness exited {harness_rc} and left no readable report "
                              f"({type(exc).__name__}: {exc}); its output is in harness.log") from exc
    if not isinstance(harness, dict):
        raise war.ReportError(f"{run['name']}: the harness report is a {type(harness).__name__}, not an object")
    arrival = harness.get("arrival") or {}
    if not arrival.get("records"):
        raise war.ReportError(f"{run['name']}: the harness report holds no arrival records "
                              f"(smoke_result={harness.get('smoke_result')}, errors={harness.get('smoke_errors')})")
    if (harness_rc != 0 or harness.get("smoke_result") != "PASS") and not drain_timeout_only(harness, harness_rc):
        raise war.ReportError(f"{run['name']}: the harness exited {harness_rc} with "
                              f"smoke_result={harness.get('smoke_result')} for a reason other than a drain timeout "
                              f"alone (errors={harness.get('smoke_errors')})")
    sink = read_sink(run_dir / "sink")
    if not run["attempt_log"]:
        rows = attempt_rows(sink)
        if rows:
            raise war.ReportError(f"{run['name']}: attempt log off, but the sink holds {rows} writer_attempt rows")
        return {"verdict": "OFF", "arrival": arrival, "sink_files": sorted(sink)}
    point = war.build_point_report(arrival, sink, loss_limit=loss_limit, util_limit=util_limit,
                                   queue_p99_limit_micros=queue_p99_limit_micros, burst_deadline_s=burst_deadline_s)
    if run.get("burst_size") and not point["burst_drain"]:
        raise war.ReportError(f"{run['name']}: a burst run whose arrival records carry no burst")
    return {"verdict": point["verdict"], "arrival": arrival, "point": point}


def unfinished(arrival):
    counts = arrival.get("counts") or {}
    return counts.get("failed", 0) + counts.get("outstanding", 0)


def run_line(run, reading):
    head = f"[{run['name']}] workers={run['workers']}"
    if reading["verdict"] == "OFF":
        counts = reading["arrival"].get("counts", {})
        return (f"{head} attempt log off; arrivals {counts.get('submitted')} completed {counts.get('completed')} "
                f"failed {counts.get('failed', 0)} outstanding {counts.get('outstanding', 0)}")
    point = reading["point"]
    loss = point["loss"]
    return (f"{head} attempts expected={loss['expected']} written={loss['written']} gaps={loss['gaps']} "
            f"dropped={loss['dropped']} loss_upper={loss['loss_upper']:.4f} utilization={point['utilization']:.3f} "
            f"queue_wait_p99_us={point['queue_wait_micros'].get('p99')} verdict={point['verdict']}"
            + (f" ({point['verdict_reason']})" if point["verdict_reason"] else "")
            + (burst_text(point["burst_drain"]) if point.get("burst_drain") else ""))


def burst_text(drain):
    p99 = drain["drain_s"].get("p99")
    return (f"; bursts={drain['bursts']} missed={drain['missed']} drain_missed={drain['drain_missed']} "
            f"refused_only={drain['refused_only']} refused_arrivals={drain['refused_arrivals']} "
            f"drain_p99_ms={'none' if p99 is None else round(p99 * 1000, 3)} deadline_ms={drain['deadline_s'] * 1000:g}")


def knee_line(burst_points, deadline_s):
    """The smallest size with a burst that drained late, whatever its refusals, or the largest size tried when none
    did. Sizes with a burst that missed only through refused arrivals are named apart."""
    missed = sorted(size for size, drain in burst_points if drain["drain_missed"])
    refused = sorted(size for size, drain in burst_points if drain["refused_only"])
    tried = sorted(size for size, _ in burst_points)
    head = f"burst-drain knee at deadline {deadline_s * 1000:g} ms: "
    line = head + (f"size {missed[0]} (first size with a burst that drained late)" if missed
                   else f"none up to size {tried[-1]}")
    if refused:
        line += f"; bursts missed only through refused arrivals at size {', '.join(map(str, refused))}"
    return line


def sink_line(conditions):
    """The sink heartbeat and settle every run used; a default daemon heartbeats every 5 minutes."""
    if len(conditions) == 1:
        hb, settle = conditions[0]
        if hb is None or settle is None:
            return "sink heartbeat and settle not recorded by the harness"
        return f"sink heartbeat {hb} ms and settle {settle} s on every run, both paired arms included"
    parts = [f"heartbeat {hb} ms settle {st} s" for hb, st in conditions]
    return "sink conditions differ between runs: " + "; ".join(parts)


def cost_line(cost):
    parts = []
    for op, entry in cost["classes"].items():
        parts.append(f"{op} p50 {entry['p50_cost_us']} us, p99 {entry['p99_cost_us']} us "
                     f"(off n={entry['off'].get('n')}, on n={entry['on'].get('n')})")
    return f"attempt-log cost, on minus off, pooled over {cost['rounds']} rounds: " + "; ".join(parts)


def dropped_latency_line(latency):
    def arm(name, stats):
        count = stats["n"]
        if not count:
            return f"{name} n=0"
        return f"{name} n={count} p50={stats['p50']} us p99={stats['p99']} us"

    parts = [
        f"{op} ({arm('off', entry['off'])}, {arm('on', entry['on'])})"
        for op, entry in latency["classes"].items()
    ]
    rounds = latency["rounds"]
    if not parts:
        return f"no completed client samples across {rounds} dropped paired rounds"
    return f"completed client latency for dropped paired rounds (count={rounds}) | " + ", ".join(parts)


def run_once(run, name, out, args, spawn, deadline_s):
    """One harness run under `name`: (record entry, reading or None when the inputs could not support a reading)."""
    run_dir = out / name
    run_dir.mkdir(parents=True, exist_ok=False)
    argv = harness_argv(run, run_dir, args)
    with open(run_dir / "harness.log", "wb") as log:
        harness_rc = spawn(argv, stdout=log, stderr=subprocess.STDOUT).returncode
    entry = {**run, "name": name, "dir": str(run_dir), "harness_rc": harness_rc}
    try:
        reading = read_run(dict(run, name=name), run_dir, loss_limit=args.loss_limit, util_limit=args.util_limit,
                           queue_p99_limit_micros=args.queue_p99_limit_micros, burst_deadline_s=deadline_s,
                           harness_rc=harness_rc)
    except war.ReportError as exc:
        return {**entry, "verdict": "BROKEN", "reason": str(exc)}, exc
    return {**entry, "verdict": reading["verdict"], "point": reading.get("point")}, reading


def run_group(group, out, args, *, spawn, deadline_s, emit):
    """Return all attempts and readings, plus the first non-VOID group, a terminal error, or None at retry exhaustion."""
    entries, seen = [], []
    for tries in range(args.void_retries + 1):
        readings = []
        for run in group:
            name = run["name"] if tries == 0 else f"{run['name']}-retry{tries}"
            entry, reading = run_once(run, name, out, args, spawn, deadline_s)
            entries.append(entry)
            if isinstance(reading, war.ReportError):
                emit(f"[{name}] BROKEN: {reading}")
                return entries, seen, reading
            emit(run_line(dict(run, name=name), reading))
            readings.append(reading)
            seen.append(reading)
        if all(reading["verdict"] != "VOID" for reading in readings):
            return entries, seen, readings
    return entries, seen, None


def execute(runs, out, args, *, spawn=subprocess.run, emit=print):
    """Run the plan in order and return (record, rc). `spawn` runs one harness argv; it is replaced in tests.

    A paired round is its two runs, off and on, adjacent in time. When either is VOID the whole round runs again in
    the same order, so the partners stay adjacent; a round still VOID after --void-retries leaves both arms, and so
    does a round in which either run had failed or outstanding arrivals, so the cost line pools clean rounds only."""
    record, pairs, bursts, seen, rc = [], {"off": [], "on": []}, [], [], 0
    dropped_pairs = {"off": [], "on": []}
    rounds = {"planned": 0, "unclean": 0}
    deadline_s = None if args.burst_deadline_ms is None else args.burst_deadline_ms / 1000
    i = 0
    while i < len(runs):
        paired = runs[i]["role"] == "pair"
        group = runs[i:i + 2] if paired else runs[i:i + 1]
        i += len(group)
        if paired and (len(group) != 2 or group[1]["role"] != "pair"
                       or group[0].get("round") != group[1].get("round")
                       or group[0]["attempt_log"] == group[1]["attempt_log"]):
            raise ValueError(f"paired runs must come as one off and one on run of the same round: {group}")
        label = f"pair-{group[0]['round']}" if paired else group[0]["name"]
        if paired:
            rounds["planned"] += 1
        entries, observed, readings = run_group(group, out, args, spawn=spawn, deadline_s=deadline_s, emit=emit)
        record.extend(entries)
        seen.extend(observed)
        if isinstance(readings, war.ReportError):
            return {"runs": record, "paired_cost": None}, 2
        if readings is None:
            what = "round" if paired else "point"
            emit(f"[{label}] still VOID after {args.void_retries} retries; no reading at this {what}"
                 + ("; the round leaves both arms" if what == "round" else ""))
            rc = 1
            continue
        if paired:
            unclean = [run["name"] for run, reading in zip(group, readings) if unfinished(reading["arrival"])]
            pool = pairs
            if unclean:
                emit(f"[{label}] {', '.join(unclean)} had failed or outstanding arrivals; the round leaves both arms")
                rounds["unclean"] += 1
                pool = dropped_pairs
            for run, reading in zip(group, readings):
                pool["on" if run["attempt_log"] else "off"].append(reading["arrival"])
        elif group[0]["role"] == "burst":
            bursts.append((group[0]["burst_size"], readings[0]["point"]["burst_drain"]))
    sink = sorted({(r["arrival"].get("sink_heartbeat_ms"), r["arrival"].get("sink_settle_s")) for r in seen},
                  key=repr)
    if sink:
        emit(sink_line(sink))
    dropped_latency = None
    if rounds["unclean"]:
        emit(f"paired arm: {rounds['unclean']} of {rounds['planned']} rounds left both arms for failed or "
             f"outstanding arrivals; the cost line covers the clean rounds only")
        dropped_latency = war.paired_latency_cost(dropped_pairs["off"], dropped_pairs["on"])
        emit(dropped_latency_line(dropped_latency))
    cost = None
    if rounds["planned"] and not (pairs["off"] or pairs["on"]):
        emit(f"paired arm: none of {rounds['planned']} rounds was clean; no cost line")
        rc = max(rc, 1)
    if pairs["off"] or pairs["on"]:
        try:
            cost = war.paired_latency_cost(pairs["off"], pairs["on"])
            emit(cost_line(cost))
        except war.ReportError as exc:
            emit(f"paired arm BROKEN: {exc}")
            rc = 2
    if bursts:
        emit(knee_line(bursts, deadline_s))
    summary = {"runs": record, "paired_cost": cost, "sink_conditions": sink, "paired_rounds": rounds}
    if dropped_latency is not None:
        summary["dropped_paired_latency"] = dropped_latency
    return summary, rc


def parse_args(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--out", required=True, help="new directory for the plan, every run and the summary")
    ap.add_argument("--workers", default="100,200,400", help="comma-separated process counts")
    ap.add_argument("--workers-per-tenant", type=int, default=5)
    ap.add_argument("--rate-per-worker", type=float, required=True, help="arrivals per second per process")
    ap.add_argument("--seconds", type=float, default=60.0, help="offered duration of each run")
    ap.add_argument("--mix", default="f7")
    ap.add_argument("--mode", choices=["real", "bench"], default="bench")
    ap.add_argument("--packs", default=None)
    ap.add_argument("--worker-timeout", type=float, default=300.0)
    ap.add_argument("--no-aa", action="store_true", help="skip the A/A pair")
    ap.add_argument("--paired-workers", type=int, default=100)
    ap.add_argument("--paired-rounds", type=int, default=3)
    ap.add_argument("--burst-sizes", default="", help="comma-separated burst sizes; empty runs no burst points")
    ap.add_argument("--burst-every", type=float, default=5.0, help="seconds between bursts")
    ap.add_argument("--burst-deadline-ms", type=float, default=None, help="drain deadline; required with bursts")
    ap.add_argument("--loss-limit", type=float, default=0.01)
    ap.add_argument("--void-retries", type=int, default=2)
    ap.add_argument("--util-limit", type=float, default=0.70)
    ap.add_argument("--queue-p99-limit-ms", type=float, default=None)
    args = ap.parse_args(argv)
    args.workers = [int(w) for w in args.workers.split(",") if w.strip()]
    args.burst_sizes = [int(b) for b in args.burst_sizes.split(",") if b.strip()]
    if args.burst_sizes and (min(args.burst_sizes) <= 0 or not math.isfinite(args.burst_every)
                             or args.burst_every <= 0 or not (args.burst_deadline_ms or 0) > 0):
        ap.error("--burst-sizes needs positive sizes, a positive --burst-every and a positive --burst-deadline-ms")
    if args.burst_sizes and max(args.burst_sizes) >= args.paired_workers:
        ap.error("--burst-sizes must stay below --paired-workers: a burst at or over the worker pool is refused by "
                 "the client whatever the writer does. A smaller burst can still meet refusals while steady arrivals "
                 "hold workers, so each run counts them per burst and the knee line names those sizes apart")
    args.queue_p99_limit_micros = None if args.queue_p99_limit_ms is None else args.queue_p99_limit_ms * 1000
    if not math.isfinite(args.rate_per_worker) or args.rate_per_worker <= 0 or args.seconds <= 0:
        ap.error("--rate-per-worker and --seconds must be positive")
    if args.paired_rounds < 0 or args.void_retries < 0:
        ap.error("--paired-rounds and --void-retries must be nonnegative")
    return args


def main(argv=None):
    args = parse_args(argv)
    out = pathlib.Path(args.out)
    out.mkdir(parents=True, exist_ok=False)
    runs = plan_runs(args.workers, aa=not args.no_aa, paired_workers=args.paired_workers,
                     paired_rounds=args.paired_rounds, burst_sizes=args.burst_sizes)
    (out / "plan.json").write_text(json.dumps({"argv": sys.argv, "runs": runs}, indent=1))
    summary, rc = execute(runs, out, args)
    (out / "summary.json").write_text(json.dumps(summary, indent=1, default=str))
    return rc


if __name__ == "__main__":
    sys.exit(main())
