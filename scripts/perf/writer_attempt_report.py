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
The daemon writes its drop counter only beside a heartbeat, so a segment holding attempt rows must carry a heartbeat
after the run window ends, or its counter does not cover the run's tail and the run is BROKEN (the harness shortens
the heartbeat and waits for one when it keeps a sink directory). A segment whose gaps exceed its drop counter is
inconsistent and makes the run BROKEN. A point whose loss_upper is
over the loss limit (default 1 percent) is VOID: rows and alarm rows share one bounded channel, so loss is likeliest
exactly at the load where the measurement matters, and a void point is re-run, never read.

Utilization counts only the rows whose `ts_utc` falls inside the run's wall-clock measurement window
(`window_utc` in the arrival report, from arrival zero to the end of the drain). Setup writes before arrival zero
(bootstrap, fixtures) share the sink file and would otherwise inflate it; they stay counted as
`outside_window_attempts`.
"""

from __future__ import annotations

import argparse
import json
import math
import pathlib
import re
import sys
from collections import Counter
from datetime import datetime

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


def fixed_variable_cost(rows):
    """Least-squares split of writer service time into a fixed part and a part per changed row.

    Fits service_micros = fixed + per_row * rows_changed over the attempts given. Rows without `rows_changed` are
    left out and counted. With fewer than two distinct row counts there is no slope to fit, and the fit says so
    instead of inventing one.
    """
    points = [(int(r["rows_changed"]), service_micros(r)) for r in rows if r.get("rows_changed") is not None]
    out = {"n": len(points), "without_rows_changed": len(rows) - len(points)}
    xs = {x for x, _ in points}
    if len(xs) < 2:
        out["fit"] = None
        out["reason"] = f"{len(xs)} distinct rows_changed value(s); a slope needs two"
        return out
    n = len(points)
    mx = sum(x for x, _ in points) / n
    my = sum(y for _, y in points) / n
    sxx = sum((x - mx) ** 2 for x, _ in points)
    sxy = sum((x - mx) * (y - my) for x, y in points)
    syy = sum((y - my) ** 2 for _, y in points)
    slope = sxy / sxx
    out["fit"] = {
        "fixed_micros": my - slope * mx,
        "per_row_micros": slope,
        "r2": (sxy * sxy) / (sxx * syy) if syy else None,
        "rows_changed_range": [min(xs), max(xs)],
    }
    return out


def parse_sink_lines(lines):
    """Split one sink file's lines into segments, each opened by a `startup` row.

    Returns a list of segments: {startup, attempts, dropped, unparsed}. Rows before the first startup row form a
    segment with startup None, so nothing is silently discarded.
    """
    segments = [{"startup": None, "attempts": [], "dropped": 0, "unparsed": 0, "heartbeat": None}]
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
            segments.append({"startup": row, "attempts": [], "dropped": 0, "unparsed": 0, "heartbeat": None})
        elif kind == "heartbeat":
            segments[-1]["heartbeat"] = row.get("ts_utc")
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


def parse_utc(text):
    try:
        moment = datetime.fromisoformat(text.replace("Z", "+00:00"))
    except (AttributeError, TypeError, ValueError):
        raise ReportError(f"timestamp {text!r} is not an RFC 3339 UTC time") from None
    if moment.tzinfo is None:
        raise ReportError(f"timestamp {text!r} carries no UTC offset")
    return moment


def join_point(arrivals, attempts, window_s, *, prefix, sequence_bits, window=None):
    """Join one point's arrivals to its writer_attempt rows by request id.

    `attempts` are every writer_attempt row of the point's sink segments. Rows whose request id carries this run's
    prefix are joined to arrivals; rows with no id (setup, background work, writes spawned without the request
    context) and rows with another prefix are counted, never dropped from the totals. With `window` (a pair of
    aware datetimes), utilization and the overall queue wait read only the rows whose `ts_utc` lies inside it.
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
            "fixed_variable_cost": fixed_variable_cost(rows),
            "queue_wait_micros": summarize([int(r.get("queue_wait_micros") or 0) for r in rows]),
            "error_rate": sum(r.get("outcome") == "error" for r in rows) / len(rows) if rows else None,
        }
    if window is None:
        in_window = attempts
    else:
        start, end = window
        in_window = [r for r in attempts if start <= parse_utc(r.get("ts_utc")) <= end]
    all_service = sum(service_micros(r) for r in in_window)
    return {
        "classes": report_classes,
        "attempts": len(attempts),
        "unscoped_attempts": unscoped,
        "foreign_attempts": foreign,
        "orphan_attempts": orphan,
        "outside_window_attempts": len(attempts) - len(in_window),
        "utilization": all_service / (window_s * 1_000_000),
        "queue_wait_micros": summarize([int(r.get("queue_wait_micros") or 0) for r in in_window]),
    }


def point_verdict(loss, joined, *, loss_limit, util_limit, queue_p99_limit_micros, counts=None):
    """VOID when loss may exceed the limit; otherwise WITHIN or OUTSIDE the stated envelope.

    An arrival that failed or was still outstanding at the drain timeout puts the point OUTSIDE: the client did not
    get every answer it asked for, whatever the writer's numbers say about the answers it did get."""
    if loss["loss_upper"] > loss_limit:
        return "VOID", f"loss upper bound {loss['loss_upper']:.4f} over {loss_limit}"
    reasons = []
    counts = counts or {}
    for state, words in (("outstanding", "outstanding at the drain timeout"), ("failed", "failed")):
        if counts.get(state):
            reasons.append(f"{counts[state]} of {counts.get('submitted')} arrivals {words}")
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


def burst_drain(records, deadline_s):
    """How long each burst of simultaneous arrivals took to clear, read from the client's own arrival records.

    A burst's drain time runs from its scheduled instant to the last finish among its completed arrivals. A burst
    MISSES when that time is over `deadline_s`, or when any of its arrivals was refused, failed or never finished:
    a refused arrival finishes at once, so leaving it out of the drain time alone would read an overload as fast.
    A burst DRAINS LATE when its completed arrivals finished past the deadline or an arrival it did not refuse never
    completed (a failed arrival counts here), whatever its refusals; `drain_missed` counts those bursts. A burst
    that missed only through refused arrivals is counted in `refused_only`: what caused those refusals is not
    measured here. `missed`, `drain_missed` and `refused_only` count bursts; `refused_arrivals` counts arrivals.
    Returns None when no record belongs to a burst.
    """
    bursts = {}
    for row in records:
        if row.get("burst") is not None:
            bursts.setdefault(row["burst"], []).append(row)
    if not bursts:
        return None
    if deadline_s is None or not deadline_s > 0:
        raise ReportError("a burst reading needs a positive drain deadline")
    per_burst = []
    for burst_id in sorted(bursts):
        rows = bursts[burst_id]
        states = Counter(row.get("state") for row in rows)
        finished = [row["finished_s"] for row in rows if row.get("state") == "completed"]
        drain = max(finished) - rows[0]["scheduled_s"] if finished else None
        unfinished = len(rows) - states["completed"]
        late = (drain is not None and drain > deadline_s) or unfinished > states["refused"]
        per_burst.append({"burst": burst_id, "size": len(rows), "completed": states["completed"],
                          "refused": states["refused"], "unfinished": unfinished, "drain_s": drain,
                          "missed": bool(unfinished) or drain is None or drain > deadline_s, "drain_late": late})
    return {
        "deadline_s": deadline_s,
        "bursts": len(per_burst),
        "sizes": sorted({b["size"] for b in per_burst}),
        "missed": sum(b["missed"] for b in per_burst),
        "refused_arrivals": sum(b["refused"] for b in per_burst),
        "drain_missed": sum(b["drain_late"] for b in per_burst),
        "refused_only": sum(b["missed"] and not b["drain_late"] for b in per_burst),
        "drain_s": summarize([b["drain_s"] for b in per_burst if b["drain_s"] is not None]),
        "per_burst": per_burst,
    }


def build_point_report(arrival_report, sink_lines_by_file, *, loss_limit=0.01, util_limit=0.70,
                       queue_p99_limit_micros=None, burst_deadline_s=None):
    prefix = arrival_report.get("request_id_prefix")
    bits = arrival_report.get("request_id_sequence_bits")
    if prefix is None or bits is None:
        raise ReportError("the arrival report carries no request id prefix; its rows cannot be joined")
    window_utc = arrival_report.get("window_utc") or {}
    if not window_utc.get("start") or not window_utc.get("end"):
        raise ReportError("the arrival report carries no wall-clock window; setup writes cannot be separated")
    window = (parse_utc(window_utc["start"]), parse_utc(window_utc["end"]))
    if not sink_lines_by_file:
        raise ReportError("no sink file was read")
    segments = []
    for name, lines in sorted(sink_lines_by_file.items()):
        for segment in parse_sink_lines(lines):
            segment["file"] = name
            segments.append(segment)
    for segment in segments:
        beat = segment["heartbeat"]
        if segment["attempts"] and (beat is None or parse_utc(beat) <= window[1]):
            raise ReportError(f"sink segment in {segment['file']} has no heartbeat after the run window ends, so its "
                              "drop counter does not cover the run")
    losses = [dict(segment_loss(s), file=s["file"]) for s in segments]
    total = {key: sum(l[key] for l in losses) for key in ("expected", "written", "gaps", "dropped")}
    denominator = total["written"] + total["dropped"]
    total["loss_upper"] = total["dropped"] / denominator if denominator else 0.0
    attempts = [row for s in segments for row in s["attempts"]]
    if not attempts:
        raise ReportError("the sink holds no writer_attempt rows; was KHIVE_WRITER_ATTEMPT_LOG set on the daemon?")
    joined = join_point(arrival_report["records"], attempts, arrival_report["observed_duration_s"],
                        prefix=prefix, sequence_bits=bits, window=window)
    verdict, reason = point_verdict(total, joined, loss_limit=loss_limit, util_limit=util_limit,
                                    queue_p99_limit_micros=queue_p99_limit_micros,
                                    counts=arrival_report.get("counts"))
    return {
        "configured_rate_per_s": arrival_report.get("configured_rate_per_s"),
        "sqlite": sink_identity(segments),
        "loss": total,
        "loss_by_segment": losses,
        **joined,
        "burst_drain": burst_drain(arrival_report["records"], burst_deadline_s),
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
    ap.add_argument("--burst-deadline-ms", type=float, default=None,
                    help="drain deadline for a run with bursts; such a run is BROKEN without one")
    args = ap.parse_args(argv)
    arrival_report = json.loads(pathlib.Path(args.arrivals).read_text())
    files = sorted(pathlib.Path(args.sink_dir).glob("writer_timeouts.*.ndjson"))
    sink = {f.name: f.read_text().splitlines() for f in files}
    try:
        report = build_point_report(
            arrival_report, sink, loss_limit=args.loss_limit, util_limit=args.util_limit,
            queue_p99_limit_micros=None if args.queue_p99_limit_ms is None else args.queue_p99_limit_ms * 1000,
            burst_deadline_s=None if args.burst_deadline_ms is None else args.burst_deadline_ms / 1000,
        )
    except ReportError as exc:
        print(json.dumps({"verdict": "BROKEN", "reason": str(exc), "sink_files": sorted(sink)}))
        return 2
    print(json.dumps(report, indent=1))
    return 0


if __name__ == "__main__":
    sys.exit(main())
