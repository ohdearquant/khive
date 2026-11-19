#!/usr/bin/env python3
"""Join a load run's arrivals to the server's per-attempt writer rows and report the writer envelope.

Inputs: the arrival records of one open-loop run (bench_load_harness.py, each carrying `request_id`) and the writer
sink NDJSON files the daemon wrote with KHIVE_WRITER_ATTEMPT_LOG=1. Every function here is pure; `main` only reads
the two inputs and prints JSON. The report states numbers and verdicts per load point; it never asserts.

Loss accounting, per sink file (one file per process and sink directory, sequence counted from 1 after each
`startup` row):
  expected     the highest attempt_seq seen in the segment (a lower bound: a lost tail leaves no later number)
  written      writer_attempt rows present in the segment
  gaps         expected - written
  dropped      the sum of `sink_error_summary` drop counts in the segment; it counts every dropped row of any kind
               and every unrouted row, so it bounds the lost attempt rows from above
  loss_upper   dropped / (written + dropped), the share of attempt rows that may be missing
A segment whose gaps exceed its drop counter is inconsistent and makes the run BROKEN. A point whose loss_upper is
over the loss limit (default 1 percent) is VOID: rows and alarm rows share one bounded channel, so loss is likeliest
exactly at the load where the measurement matters, and a void point is re-run, never read.
"""

from __future__ import annotations

import argparse
import json
import math
import pathlib
import re
import sys

ATTEMPT_KIND = "writer_attempt"
DROP_MESSAGE = re.compile(r"^sink drops since last summary: (\d+)$")
SERVICE_KEYS = ("transaction_acquire_micros", "body_micros", "commit_micros")


class ReportError(ValueError):
    """The inputs cannot support a reading; the report is BROKEN, never empty."""


def nearest_rank(sorted_values, q):
    if not sorted_values:
        return None
    return sorted_values[min(len(sorted_values) - 1, int(q * len(sorted_values)))]


def summarize(values):
    values = sorted(values)
    n = len(values)
    if n == 0:
        return {"n": 0}
    mean = sum(values) / n
    variance = sum((v - mean) ** 2 for v in values) / n
    return {
        "n": n,
        "mean": mean,
        "cv": math.sqrt(variance) / mean if mean else None,
        "p50": nearest_rank(values, 0.50),
        "p95": nearest_rank(values, 0.95),
        "p99": nearest_rank(values, 0.99),
        "max": values[-1],
    }


def parse_sink_lines(lines):
    """Split one sink file's lines into segments, each opened by a `startup` row.

    Returns a list of segments: {startup, attempts, dropped, unparsed}. Rows before the first startup row form a
    segment with startup None, so nothing is silently discarded.
    """
    segments = [{"startup": None, "attempts": [], "dropped": 0, "unparsed": 0}]
    for line in lines:
        if not line.strip():
            continue
        try:
            row = json.loads(line)
        except json.JSONDecodeError:
            segments[-1]["unparsed"] += 1
            continue
        kind = row.get("kind")
        if kind == "startup":
            segments.append({"startup": row, "attempts": [], "dropped": 0, "unparsed": 0})
        elif kind == ATTEMPT_KIND:
            segments[-1]["attempts"].append(row)
        elif kind == "sink_error_summary":
            match = DROP_MESSAGE.match(row.get("error") or "")
            if match:
                segments[-1]["dropped"] += int(match.group(1))
    return [s for s in segments if s["startup"] is not None or s["attempts"] or s["dropped"] or s["unparsed"]]


def segment_loss(segment):
    seqs = [row["attempt_seq"] for row in segment["attempts"] if isinstance(row.get("attempt_seq"), int)]
    if len(seqs) != len(segment["attempts"]):
        raise ReportError("a writer_attempt row carries no integer attempt_seq")
    if len(set(seqs)) != len(seqs):
        raise ReportError("a sink segment repeats an attempt_seq; it is not one writer's sequence")
    expected = max(seqs) if seqs else 0
    written = len(seqs)
    gaps = expected - written
    dropped = segment["dropped"]
    if gaps > dropped:
        raise ReportError(f"sink segment has {gaps} sequence gaps but its drop counter reads {dropped}")
    total = written + dropped
    return {
        "expected": expected,
        "written": written,
        "gaps": gaps,
        "dropped": dropped,
        "loss_upper": dropped / total if total else 0.0,
    }


def service_micros(row):
    return sum(int(row.get(key) or 0) for key in SERVICE_KEYS)


def join_point(arrivals, attempts, window_s, *, prefix, sequence_bits):
    """Join one point's arrivals to its writer_attempt rows by request id.

    `attempts` are every writer_attempt row of the point's sink segments. Rows whose request id carries this run's
    prefix are joined to arrivals; rows with no id (setup, background work, writes spawned without the request
    context) and rows with another prefix are counted, never dropped from the totals.
    """
    if window_s <= 0:
        raise ReportError("the measurement window must be positive")
    by_id = {}
    unscoped = 0
    foreign = 0
    for row in attempts:
        rid = row.get("request_id")
        if rid is None:
            unscoped += 1
        elif rid >> sequence_bits != prefix:
            foreign += 1
        else:
            by_id.setdefault(rid, []).append(row)
    arrival_ids = {a["request_id"] for a in arrivals if a.get("request_id") is not None}
    orphan = sum(len(rows) for rid, rows in by_id.items() if rid not in arrival_ids)

    classes = {}
    for arrival in arrivals:
        op = arrival["op"]
        entry = classes.setdefault(op, {"arrivals": 0, "completed": 0, "joined": 0, "attempts": [], "per_request": []})
        entry["arrivals"] += 1
        if arrival.get("state") != "completed":
            continue
        entry["completed"] += 1
        rows = by_id.get(arrival.get("request_id"), [])
        if rows:
            entry["joined"] += 1
            entry["per_request"].append(len(rows))
            entry["attempts"].extend(rows)

    report_classes = {}
    for op, entry in sorted(classes.items()):
        rows = entry["attempts"]
        report_classes[op] = {
            "arrivals": entry["arrivals"],
            "completed": entry["completed"],
            "joined": entry["joined"],
            "joined_fraction": entry["joined"] / entry["completed"] if entry["completed"] else None,
            "attempts_per_request": summarize(entry["per_request"]),
            "service_micros": summarize([service_micros(r) for r in rows]),
            "queue_wait_micros": summarize([int(r.get("queue_wait_micros") or 0) for r in rows]),
            "error_rate": sum(r.get("outcome") == "error" for r in rows) / len(rows) if rows else None,
        }
    all_service = sum(service_micros(r) for r in attempts)
    return {
        "classes": report_classes,
        "attempts": len(attempts),
        "unscoped_attempts": unscoped,
        "foreign_attempts": foreign,
        "orphan_attempts": orphan,
        "utilization": all_service / (window_s * 1_000_000),
        "queue_wait_micros": summarize([int(r.get("queue_wait_micros") or 0) for r in attempts]),
    }


def point_verdict(loss, joined, *, loss_limit, util_limit, queue_p99_limit_micros):
    """VOID when loss may exceed the limit; otherwise WITHIN or OUTSIDE the stated envelope."""
    if loss["loss_upper"] > loss_limit:
        return "VOID", f"loss upper bound {loss['loss_upper']:.4f} over {loss_limit}"
    reasons = []
    if joined["utilization"] >= util_limit:
        reasons.append(f"utilization {joined['utilization']:.3f} >= {util_limit}")
    p99 = joined["queue_wait_micros"].get("p99")
    if queue_p99_limit_micros is not None and p99 is not None and p99 >= queue_p99_limit_micros:
        reasons.append(f"queue wait p99 {p99} us >= {queue_p99_limit_micros} us")
    return ("OUTSIDE", "; ".join(reasons)) if reasons else ("WITHIN", "")


def sink_identity(segments):
    startups = [s["startup"] for s in segments if s["startup"] is not None]
    identities = {json.dumps(row.get("sqlite"), sort_keys=True) for row in startups}
    if len(identities) > 1:
        raise ReportError("sink segments name different SQLite builds")
    return startups[0].get("sqlite") if startups else None


def build_point_report(arrival_report, sink_lines_by_file, *, loss_limit=0.01, util_limit=0.70,
                       queue_p99_limit_micros=None):
    prefix = arrival_report.get("request_id_prefix")
    bits = arrival_report.get("request_id_sequence_bits")
    if prefix is None or bits is None:
        raise ReportError("the arrival report carries no request id prefix; its rows cannot be joined")
    if not sink_lines_by_file:
        raise ReportError("no sink file was read")
    segments = []
    for name, lines in sorted(sink_lines_by_file.items()):
        for segment in parse_sink_lines(lines):
            segment["file"] = name
            segments.append(segment)
    losses = [dict(segment_loss(s), file=s["file"]) for s in segments]
    total = {key: sum(l[key] for l in losses) for key in ("expected", "written", "gaps", "dropped")}
    denominator = total["written"] + total["dropped"]
    total["loss_upper"] = total["dropped"] / denominator if denominator else 0.0
    attempts = [row for s in segments for row in s["attempts"]]
    if not attempts:
        raise ReportError("the sink holds no writer_attempt rows; was KHIVE_WRITER_ATTEMPT_LOG set on the daemon?")
    joined = join_point(arrival_report["records"], attempts, arrival_report["observed_duration_s"],
                        prefix=prefix, sequence_bits=bits)
    verdict, reason = point_verdict(total, joined, loss_limit=loss_limit, util_limit=util_limit,
                                    queue_p99_limit_micros=queue_p99_limit_micros)
    return {
        "configured_rate_per_s": arrival_report.get("configured_rate_per_s"),
        "sqlite": sink_identity(segments),
        "loss": total,
        "loss_by_segment": losses,
        **joined,
        "verdict": verdict,
        "verdict_reason": reason,
        "limits": {"loss": loss_limit, "utilization": util_limit, "queue_wait_p99_micros": queue_p99_limit_micros},
    }


def paired_latency_cost(off_runs, on_runs):
    """Client latency with the attempt log off and on, pooled over adjacent rounds, per op class.

    Each run is an arrival report; only completed arrivals count. The cost is stated as on minus off at p50 and
    p99, so a reader sees what the measurement mode adds to the latency it measures.
    """
    if not off_runs or len(off_runs) != len(on_runs):
        raise ReportError("the paired arm needs the same positive number of off and on rounds")

    def pooled(runs):
        by_op = {}
        for run in runs:
            for row in run["records"]:
                if row.get("state") == "completed" and row.get("latency_us") is not None:
                    by_op.setdefault(row["op"], []).append(row["latency_us"])
        return by_op

    off, on = pooled(off_runs), pooled(on_runs)
    out = {}
    for op in sorted(set(off) | set(on)):
        a, b = summarize(off.get(op, [])), summarize(on.get(op, []))
        out[op] = {
            "off": a,
            "on": b,
            "p50_cost_us": b["p50"] - a["p50"] if a.get("n") and b.get("n") else None,
            "p99_cost_us": b["p99"] - a["p99"] if a.get("n") and b.get("n") else None,
        }
    return {"rounds": len(off_runs), "classes": out}


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--arrivals", required=True, help="JSON file holding one open-loop arrival report")
    ap.add_argument("--sink-dir", required=True, help="directory holding the daemon's writer_timeouts.*.ndjson")
    ap.add_argument("--loss-limit", type=float, default=0.01)
    ap.add_argument("--util-limit", type=float, default=0.70)
    ap.add_argument("--queue-p99-limit-ms", type=float, default=None)
    args = ap.parse_args(argv)
    arrival_report = json.loads(pathlib.Path(args.arrivals).read_text())
    files = sorted(pathlib.Path(args.sink_dir).glob("writer_timeouts.*.ndjson"))
    sink = {f.name: f.read_text().splitlines() for f in files}
    try:
        report = build_point_report(
            arrival_report, sink, loss_limit=args.loss_limit, util_limit=args.util_limit,
            queue_p99_limit_micros=None if args.queue_p99_limit_ms is None else args.queue_p99_limit_ms * 1000,
        )
    except ReportError as exc:
        print(json.dumps({"verdict": "BROKEN", "reason": str(exc), "sink_files": sorted(sink)}))
        return 2
    print(json.dumps(report, indent=1))
    return 0


if __name__ == "__main__":
    sys.exit(main())
