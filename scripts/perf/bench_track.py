#!/usr/bin/env python3
"""Trend ledger for khive bench suites (stdlib-only).

Appends one JSONL record per (suite, commit) to bench-data/<suite>.jsonl and
renders a compact markdown trend summary (last N runs, per-metric direction
arrows). This is purely observational per the bench-program spec's
blocking-promotion ladder (docs `.khive/workspaces/20260710/bench-program/
SPEC-draft.md`) - it never asserts pass/fail and never places a threshold.
That is `bench_calibrate.py`'s job for calibration and, eventually, a real
CI gate's job for enforcement.

Three metric sources, one record shape:
  calibrate   reuse scripts/perf/bench_calibrate.py's SUITES registry
              (pipeline, load) - runs the suite's build_cmd once via
              bench_calibrate._run_once and reuses its extractor. Does not
              duplicate the pipeline/load stdout-parsing regexes.
  json        flatten every numeric leaf out of an arbitrary bench JSON file
              (e.g. the BENCH_JSON scripts/bench_1m.sh --ci-synthetic writes).
  criterion   walk a `cargo bench` --quick output tree
              (target/criterion/**/{new,base}/estimates.json) and extract
              mean/median/std_dev point estimates (nanoseconds) per bench id.

Record shape (schema_version 1):
  {schema_version, suite, sha, branch, timestamp, metrics, host}

`timestamp` is the commit's own commit-date (`git show -s --format=%cI`), not
wall-clock `time.time()` - two CI runs at the same SHA (e.g. a re-run) then
carry an identical, reproducible timestamp instead of drifting with runner
scheduling.
"""

from __future__ import annotations

import argparse
import json
import os
import pathlib
import platform
import subprocess
import sys

sys.path.insert(0, str(pathlib.Path(__file__).parent))
import bench_calibrate as calibrate  # noqa: E402  (path insert must precede this)

SCHEMA_VERSION = 1
REPO_ROOT = calibrate.REPO_ROOT
DATA_DIR = REPO_ROOT / "bench-data"

CRITERION_ESTIMATE_KEYS = ("mean", "median", "std_dev", "slope")


def _git_sha() -> str:
    return calibrate._git_sha()


def _commit_timestamp(sha: str) -> str:
    """The commit's own commit-date, ISO8601. Falls back to wall-clock only
    when git cannot resolve `sha` (e.g. a synthetic sha in a test, or a
    shallow checkout missing the commit) - "from git commit not wall clock
    where possible" per the trend-ledger schema.
    """
    out = subprocess.run(
        ["git", "show", "-s", "--format=%cI", sha],
        cwd=REPO_ROOT,
        capture_output=True,
        text=True,
    )
    if out.returncode == 0 and out.stdout.strip():
        return out.stdout.strip()
    return calibrate._iso_now()


def _current_branch() -> str:
    ref = os.environ.get("GITHUB_REF_NAME")
    if ref:
        return ref
    out = subprocess.run(
        ["git", "rev-parse", "--abbrev-ref", "HEAD"],
        cwd=REPO_ROOT,
        capture_output=True,
        text=True,
        check=True,
    )
    return out.stdout.strip()


def host_fingerprint() -> dict:
    return {
        "os": platform.system(),
        "arch": platform.machine(),
        "python": platform.python_version(),
        "cpu_count": os.cpu_count(),
        "runner": os.environ.get("RUNNER_NAME", "local"),
    }


# ── metric sources ──────────────────────────────────────────────────────────


def collect_calibrate_metrics(
    suite_name: str, extra_args: list[str], run_dir: pathlib.Path
) -> dict[str, float]:
    """Run a scripts/perf/bench_calibrate.py SUITES entry exactly once.

    Deliberately does NOT call `bench_calibrate._run_once`: that function is
    fail-closed by design (a nonzero child exit raises `ChildFailure` before
    any metric extraction happens), which is correct for calibration - a
    crashed run must never pollute a same-SHA variance profile. A tracker
    has the opposite job: it is Advisory (no thresholds, per the
    bench-program spec's promotion ladder), so a suite whose own internal
    recall/precision gate returns FAIL is exactly the interesting data point
    to record, not a reason to record nothing. `_run_once_no_gate` below
    reuses the suite's own `build_cmd`/`extract` pair (the pipeline/load
    stdout and JSON parsing logic is not duplicated here) but always
    attempts extraction regardless of the child's exit code.
    """
    if suite_name not in calibrate.SUITES:
        raise SystemExit(
            f"'{suite_name}' is not a bench_calibrate suite (known: "
            f"{sorted(calibrate.SUITES)}); use --source json or --source criterion instead."
        )
    suite = calibrate.SUITES[suite_name]
    run_dir.mkdir(parents=True, exist_ok=True)
    args = [*suite["default_args"], *extra_args]
    metrics, wall_s = _run_once_no_gate(suite_name, run_dir, args)
    metrics = dict(metrics)
    metrics["_wall_s"] = wall_s
    return metrics


def _run_once_no_gate(
    suite_name: str, run_dir: pathlib.Path, extra_args: list[str]
) -> tuple[dict[str, float], float]:
    """Run a bench_calibrate SUITES entry once, extracting metrics no matter
    what the child process's own exit code or internal threshold verdict
    was. No threshold logic lives in this tracking path - a suite's exit
    code is recorded as the `_exit_code` metric for visibility, never used
    to gate whether the run's metrics get recorded.

    Process management (isolated HOME, process-group timeout kill) mirrors
    `bench_calibrate._run_once`; only the "abort on nonzero exit" policy is
    intentionally different.
    """
    suite = calibrate.SUITES[suite_name]
    argv = suite["build_cmd"](run_dir, extra_args)

    home_dir = run_dir / "home"
    home_dir.mkdir(parents=True, exist_ok=True)
    env = {**os.environ}
    env["HOME"] = str(home_dir)

    t0 = calibrate.time.time()
    proc = subprocess.Popen(
        argv,
        cwd=str(calibrate.REPO_ROOT),
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        start_new_session=True,
    )
    pgid = os.getpgid(proc.pid)
    try:
        stdout, stderr = proc.communicate(timeout=suite["timeout_s"])
    except subprocess.TimeoutExpired:
        calibrate._kill_process_tree(pgid, proc)
        try:
            stdout, stderr = proc.communicate(timeout=10)
        except subprocess.TimeoutExpired:
            stdout, stderr = "", ""
        wall_s = calibrate.time.time() - t0
        (run_dir / "stdout.log").write_text(stdout or "")
        (run_dir / "stderr.log").write_text(stderr or "")
        (run_dir / "argv.txt").write_text(" ".join(argv) + "\n")
        raise
    wall_s = calibrate.time.time() - t0

    (run_dir / "stdout.log").write_text(stdout)
    (run_dir / "stderr.log").write_text(stderr)
    (run_dir / "argv.txt").write_text(" ".join(argv) + "\n")

    cp = subprocess.CompletedProcess(argv, proc.returncode, stdout, stderr)
    # Extraction runs regardless of proc.returncode - a nonzero exit caused
    # by the suite's own internal threshold (e.g. bench_pipeline_daemon.py's
    # "Gate: FAIL" -> exit 1) still has fully-formed stdout to extract from.
    # A genuinely broken run (crash before any output) still surfaces loudly
    # here via SchemaError from the extractor finding no expected output.
    metrics = suite["extract"](run_dir, cp)
    if not metrics:
        raise SystemExit(
            f"suite '{suite_name}' extractor returned no metrics; see {run_dir}/stdout.log"
        )
    metrics = dict(metrics)
    metrics["_exit_code"] = float(proc.returncode)
    return metrics, wall_s


def _flatten_numeric_with_lists(prefix: str, obj, out: dict[str, float]) -> None:
    """Like `bench_calibrate._flatten_numeric`, but also indexes into lists
    by position (e.g. `bench_1m.sh`'s BENCH_JSON has a `rows` array, one
    entry per N-point) - the `load` suite's report has no list-of-numeric
    shape to flatten, so bench_calibrate's helper never needed this; the
    `bench-1m` JSON does, so this is a small local extension, not a fork of
    that helper's dict/scalar handling.
    """
    if isinstance(obj, dict):
        for k, v in obj.items():
            _flatten_numeric_with_lists(f"{prefix}.{k}" if prefix else str(k), v, out)
    elif isinstance(obj, list):
        for i, v in enumerate(obj):
            _flatten_numeric_with_lists(f"{prefix}.{i}" if prefix else str(i), v, out)
    elif isinstance(obj, bool):
        return
    elif isinstance(obj, (int, float)):
        out[prefix] = float(obj)


def collect_json_metrics(json_path: pathlib.Path, prefix: str = "") -> dict[str, float]:
    """Flatten every numeric leaf of an arbitrary bench JSON file (dicts and
    lists alike - see `_flatten_numeric_with_lists`)."""
    data = json.loads(json_path.read_text())
    metrics: dict[str, float] = {}
    _flatten_numeric_with_lists(prefix, data, metrics)
    if not metrics:
        raise SystemExit(f"no numeric metrics found in {json_path}")
    return metrics


def _criterion_bench_id(estimates_path: pathlib.Path, criterion_dir: pathlib.Path) -> str:
    # .../target/criterion/<group>/.../{new,base}/estimates.json
    rel = estimates_path.relative_to(criterion_dir)
    parts = rel.parts[:-2]  # drop "{new,base}/estimates.json"
    return "/".join(parts)


def collect_criterion_metrics(criterion_dir: pathlib.Path) -> dict[str, float]:
    """Walk a `cargo bench` output tree and extract per-bench point estimates.

    Prefers `new/estimates.json` (always written by the run that just
    happened); falls back to `base/estimates.json` for a bench id that only
    has a prior baseline (should not normally occur in a fresh CI checkout,
    but keeps this tolerant of a locally-primed target/ dir).
    """
    metrics: dict[str, float] = {}
    seen_ids: set[str] = set()
    candidates = sorted(criterion_dir.rglob("new/estimates.json")) + sorted(
        criterion_dir.rglob("base/estimates.json")
    )
    for estimates_path in candidates:
        bench_id = _criterion_bench_id(estimates_path, criterion_dir)
        if bench_id in seen_ids:
            continue
        seen_ids.add(bench_id)
        try:
            data = json.loads(estimates_path.read_text())
        except (OSError, json.JSONDecodeError):
            continue
        for key in CRITERION_ESTIMATE_KEYS:
            entry = data.get(key)
            if isinstance(entry, dict) and isinstance(entry.get("point_estimate"), (int, float)):
                metrics[f"{bench_id}.{key}_ns"] = float(entry["point_estimate"])
    if not metrics:
        raise SystemExit(
            f"no criterion estimates.json found under {criterion_dir} - did `cargo bench` run?"
        )
    return metrics


# ── record + ledger ──────────────────────────────────────────────────────────


def build_record(suite: str, metrics: dict[str, float], sha: str, branch: str) -> dict:
    return {
        "schema_version": SCHEMA_VERSION,
        "suite": suite,
        "sha": sha,
        "branch": branch,
        "timestamp": _commit_timestamp(sha),
        "metrics": metrics,
        "host": host_fingerprint(),
    }


def ledger_path(suite: str, data_dir: pathlib.Path = DATA_DIR) -> pathlib.Path:
    return data_dir / f"{suite}.jsonl"


def append_record(record: dict, data_dir: pathlib.Path = DATA_DIR) -> pathlib.Path:
    data_dir.mkdir(parents=True, exist_ok=True)
    path = ledger_path(record["suite"], data_dir)
    with path.open("a") as fh:
        fh.write(json.dumps(record, sort_keys=True) + "\n")
    return path


def read_records(suite: str, data_dir: pathlib.Path = DATA_DIR) -> list[dict]:
    path = ledger_path(suite, data_dir)
    if not path.exists():
        return []
    records = []
    for line in path.read_text().splitlines():
        line = line.strip()
        if line:
            records.append(json.loads(line))
    return records


# ── trend rendering ──────────────────────────────────────────────────────────


def _arrow(prev: float, curr: float) -> str:
    if curr > prev:
        return "^ up"
    if curr < prev:
        return "v down"
    return "= flat"


def render_trend_markdown(suite: str, limit: int = 10, data_dir: pathlib.Path = DATA_DIR) -> str:
    all_records = read_records(suite, data_dir)
    lines = [f"# Bench trend: `{suite}`", ""]
    if not all_records:
        lines.append("No history yet.")
        lines.append("")
        return "\n".join(lines)

    window = all_records[-limit:]
    latest = window[-1]
    lines.append(f"- runs in window: {len(window)} (of {len(all_records)} total)")
    lines.append(f"- latest sha: `{latest['sha'][:8]}` ({latest['branch']}) at {latest['timestamp']}")
    lines.append("")
    lines.append(
        "Informational only - no thresholds, per the bench-program spec's promotion ladder. "
        "Direction arrows compare the latest run to the previous run in this window."
    )
    lines.append("")
    lines.append("| metric | latest | previous | direction | min (window) | max (window) |")
    lines.append("|---|---|---|---|---|---|")

    metric_names = sorted(latest["metrics"])
    for name in metric_names:
        series = [r["metrics"][name] for r in window if name in r["metrics"]]
        if not series:
            continue
        curr = series[-1]
        prev = series[-2] if len(series) > 1 else curr
        direction = _arrow(prev, curr)
        lines.append(
            f"| {name} | {curr:.4g} | {prev:.4g} | {direction} | {min(series):.4g} | {max(series):.4g} |"
        )
    lines.append("")
    return "\n".join(lines)


# ── CLI ──────────────────────────────────────────────────────────────────────


def _cmd_record(args: argparse.Namespace) -> int:
    sha = args.sha or _git_sha()
    branch = args.branch or _current_branch()

    if args.source == "calibrate":
        run_dir = pathlib.Path(args.run_dir or (DATA_DIR / "runs" / f"{args.suite}_{sha[:8]}"))
        metrics = collect_calibrate_metrics(args.suite, args.extra_arg, run_dir)
    elif args.source == "json":
        if not args.json_file:
            raise SystemExit("--source json requires --json-file")
        metrics = collect_json_metrics(pathlib.Path(args.json_file), prefix=args.json_prefix)
    elif args.source == "criterion":
        criterion_dir = pathlib.Path(args.criterion_dir or (REPO_ROOT / "crates" / "target" / "criterion"))
        metrics = collect_criterion_metrics(criterion_dir)
    else:  # pragma: no cover - argparse choices already constrain this
        raise SystemExit(f"unknown source {args.source!r}")

    record = build_record(args.suite, metrics, sha, branch)
    path = append_record(record, pathlib.Path(args.data_dir) if args.data_dir else DATA_DIR)
    print(f"[bench_track] appended {len(metrics)} metrics for suite={args.suite} sha={sha[:8]} -> {path}")

    md = render_trend_markdown(args.suite, limit=args.limit, data_dir=path.parent)
    if args.summary_out:
        pathlib.Path(args.summary_out).write_text(md)
        print(f"[bench_track] wrote trend summary -> {args.summary_out}")
    else:
        print(md)
    return 0


def _cmd_render(args: argparse.Namespace) -> int:
    md = render_trend_markdown(
        args.suite, limit=args.limit, data_dir=pathlib.Path(args.data_dir) if args.data_dir else DATA_DIR
    )
    if args.out:
        pathlib.Path(args.out).write_text(md)
        print(f"[bench_track] wrote trend summary -> {args.out}")
    else:
        print(md)
    return 0


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)

    rec = sub.add_parser("record", help="run one suite once and append a trend-ledger record")
    rec.add_argument("--suite", required=True, help="ledger name, e.g. pipeline, load, bench-1m, components")
    rec.add_argument("--source", required=True, choices=["calibrate", "json", "criterion"])
    rec.add_argument("--json-file", help="(--source json) path to a bench JSON file to flatten")
    rec.add_argument("--json-prefix", default="", help="(--source json) key prefix for flattened metrics")
    rec.add_argument("--criterion-dir", help="(--source criterion) target/criterion dir to walk")
    rec.add_argument(
        "--extra-arg",
        action="append",
        default=[],
        help="(--source calibrate) extra argv token appended to the suite's default args (repeatable)",
    )
    rec.add_argument("--run-dir", help="(--source calibrate) scratch dir for the single run's artifacts")
    rec.add_argument("--sha", help="commit sha to record (default: current HEAD)")
    rec.add_argument("--branch", help="branch name to record (default: $GITHUB_REF_NAME or current branch)")
    rec.add_argument("--data-dir", help="ledger directory (default: bench-data/)")
    rec.add_argument("--limit", type=int, default=10, help="runs to include in the rendered trend (default: 10)")
    rec.add_argument("--summary-out", help="write the rendered trend markdown to this path")
    rec.set_defaults(func=_cmd_record)

    ren = sub.add_parser("render", help="render the trend markdown for an existing ledger")
    ren.add_argument("--suite", required=True)
    ren.add_argument("--data-dir", help="ledger directory (default: bench-data/)")
    ren.add_argument("--limit", type=int, default=10)
    ren.add_argument("--out", help="write to this path instead of stdout")
    ren.set_defaults(func=_cmd_render)

    args = ap.parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
