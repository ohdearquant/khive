#!/usr/bin/env python3
"""Run the F7 writer sweep: one load-harness run per point, each against its own scratch daemon.

The run order is fixed before the first run and written to plan.json in the output directory:

  1. A/A: the first process count twice, back to back, attempt log on. The difference between the two bounds the
     noise of the harness itself at that count.
  2. Sweep: every process count once, attempt log on.
  3. Paired arm at --paired-workers: --paired-rounds rounds of one run with the attempt log off and one with it on,
     adjacent in time. The order flips each round (off-on, on-off, ...) so drift does not land on one arm. Client
     latency is pooled per arm over the rounds, and the cost line states on minus off at p50 and p99 per op class.

Each run gets its own directory under --out with the harness report (harness.json), the harness log and the sink
directory the daemon wrote (sink/), which is kept. Every attempt-log run prints the attempts expected, rows written,
sequence gaps, drop counter and loss bound, then its point verdict (writer_attempt_report.py). A run whose loss bound
is over --loss-limit is VOID and runs again, up to --void-retries more times; the void runs stay on the record. A run
with the attempt log off must leave no writer_attempt row in its sink: one there means the variable reached a daemon
it should not have, and the paired arm is BROKEN.

The script reports; it never asserts an envelope. Exit 0 when every point has a reading, 1 when a point is still
VOID after its retries, 2 when an input could not support a reading (BROKEN).
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


def plan_runs(workers, *, aa, paired_workers, paired_rounds):
    """The ordered list of runs: A/A pair at the first count, the sweep, then the alternating paired arm."""
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
    return argv


def read_sink(sink_dir):
    files = sorted(pathlib.Path(sink_dir).glob("writer_timeouts.*.ndjson"))
    return {f.name: f.read_text().splitlines() for f in files}


def attempt_rows(sink):
    return sum(len(segment["attempts"]) for lines in sink.values() for segment in war.parse_sink_lines(lines))


def read_run(run, run_dir, *, loss_limit, util_limit, queue_p99_limit_micros):
    """One run's reading: the point report for a log-on run, client latency and a row count for a log-off run."""
    harness = json.loads((run_dir / "harness.json").read_text())
    arrival = harness.get("arrival") or {}
    if not arrival.get("records"):
        raise war.ReportError(f"{run['name']}: the harness report holds no arrival records "
                              f"(smoke_result={harness.get('smoke_result')}, errors={harness.get('smoke_errors')})")
    sink = read_sink(run_dir / "sink")
    if not run["attempt_log"]:
        rows = attempt_rows(sink)
        if rows:
            raise war.ReportError(f"{run['name']}: attempt log off, but the sink holds {rows} writer_attempt rows")
        return {"verdict": "OFF", "arrival": arrival, "sink_files": sorted(sink)}
    point = war.build_point_report(arrival, sink, loss_limit=loss_limit, util_limit=util_limit,
                                   queue_p99_limit_micros=queue_p99_limit_micros)
    return {"verdict": point["verdict"], "arrival": arrival, "point": point}


def run_line(run, reading):
    head = f"[{run['name']}] workers={run['workers']}"
    if reading["verdict"] == "OFF":
        counts = reading["arrival"].get("counts", {})
        return f"{head} attempt log off; arrivals {counts.get('submitted')} completed {counts.get('completed')}"
    point = reading["point"]
    loss = point["loss"]
    return (f"{head} attempts expected={loss['expected']} written={loss['written']} gaps={loss['gaps']} "
            f"dropped={loss['dropped']} loss_upper={loss['loss_upper']:.4f} utilization={point['utilization']:.3f} "
            f"queue_wait_p99_us={point['queue_wait_micros'].get('p99')} verdict={point['verdict']}"
            + (f" ({point['verdict_reason']})" if point["verdict_reason"] else ""))


def cost_line(cost):
    parts = []
    for op, entry in cost["classes"].items():
        parts.append(f"{op} p50 {entry['p50_cost_us']} us, p99 {entry['p99_cost_us']} us "
                     f"(off n={entry['off'].get('n')}, on n={entry['on'].get('n')})")
    return f"attempt-log cost, on minus off, pooled over {cost['rounds']} rounds: " + "; ".join(parts)


def execute(runs, out, args, *, spawn=subprocess.run, emit=print):
    """Run the plan in order and return (record, rc). `spawn` runs one harness argv; it is replaced in tests."""
    record, pairs, rc = [], {"off": [], "on": []}, 0
    for run in runs:
        tries = 0
        while True:
            name = run["name"] if tries == 0 else f"{run['name']}-retry{tries}"
            run_dir = out / name
            run_dir.mkdir(parents=True, exist_ok=False)
            argv = harness_argv(run, run_dir, args)
            with open(run_dir / "harness.log", "wb") as log:
                harness_rc = spawn(argv, stdout=log, stderr=subprocess.STDOUT).returncode
            try:
                reading = read_run(dict(run, name=name), run_dir, loss_limit=args.loss_limit,
                                   util_limit=args.util_limit, queue_p99_limit_micros=args.queue_p99_limit_micros)
            except war.ReportError as exc:
                emit(f"[{name}] BROKEN: {exc}")
                record.append({**run, "name": name, "dir": str(run_dir), "harness_rc": harness_rc,
                               "verdict": "BROKEN", "reason": str(exc)})
                return {"runs": record, "paired_cost": None}, 2
            emit(run_line(dict(run, name=name), reading))
            record.append({**run, "name": name, "dir": str(run_dir), "harness_rc": harness_rc,
                           "verdict": reading["verdict"], "point": reading.get("point")})
            if reading["verdict"] != "VOID":
                break
            tries += 1
            if tries > args.void_retries:
                emit(f"[{run['name']}] still VOID after {args.void_retries} retries; no reading at this point")
                rc = 1
                break
        if run["role"] == "pair" and reading["verdict"] != "VOID":
            pairs["on" if run["attempt_log"] else "off"].append(reading["arrival"])
    cost = None
    if pairs["off"] or pairs["on"]:
        try:
            cost = war.paired_latency_cost(pairs["off"], pairs["on"])
            emit(cost_line(cost))
        except war.ReportError as exc:
            emit(f"paired arm BROKEN: {exc}")
            rc = 2
    return {"runs": record, "paired_cost": cost}, rc


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
    ap.add_argument("--loss-limit", type=float, default=0.01)
    ap.add_argument("--void-retries", type=int, default=2)
    ap.add_argument("--util-limit", type=float, default=0.70)
    ap.add_argument("--queue-p99-limit-ms", type=float, default=None)
    args = ap.parse_args(argv)
    args.workers = [int(w) for w in args.workers.split(",") if w.strip()]
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
                     paired_rounds=args.paired_rounds)
    (out / "plan.json").write_text(json.dumps({"argv": sys.argv, "runs": runs}, indent=1))
    summary, rc = execute(runs, out, args)
    (out / "summary.json").write_text(json.dumps(summary, indent=1, default=str))
    return rc


if __name__ == "__main__":
    sys.exit(main())
