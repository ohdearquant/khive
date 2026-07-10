#!/usr/bin/env python3
"""Same-SHA null-distribution calibration harness.

Runs a chosen bench suite K times at the CURRENT commit (unchanged) and
emits a variance profile per metric: samples, mean, std, min, max, CV, and
an ADVISORY floor at mean-3*std. This harness never sets a gate threshold
itself — it measures run-to-run noise at a fixed SHA so a human can place a
blocking floor with a known noise budget behind it.

Motivation: bench-1m.yml (recall@10 >= 0.90) and bench-pipeline (P@K >=
0.70), plus the nine load-harness gate dimensions in bench_load_harness.py,
all carry thresholds that were never calibrated against same-commit noise.
Rule: no blocking threshold without a measured null distribution.

Suites (pluggable via SUITES registry):
  pipeline  drives scripts/perf/bench_pipeline_daemon.py (kkernel-bench,
            bench-embedder feature). Extracts the mean fused/vector/keyword
            P@K, p50/p95 latency, and per-query P@K/vec/kw/p50 breakdown
            from its stdout (it does not print JSON; it does append a row
            to the gitignored perf/pipeline-ledger.csv, which is left
            untouched by this harness).
  load      drives scripts/perf/bench_load_harness.py with cheap
            --mode bench defaults (small worker/tenant counts, no GPU
            lock). Extracts every numeric leaf under its --report JSON
            "dimensions" tree (the nine reported gate dimensions) plus
            op_counts / op_error_counts. Non-numeric leaves (status
            strings, per-tenant attribution detail) are not statted; they
            are still visible in the per-run stdout/report.json artifacts
            kept under --out/runs/.

Safety:
  - Refuses to run against a dirty git worktree (the whole point is
    same-SHA noise measurement; a dirty tree would conflate code-change
    variance with run-to-run noise). Override with --allow-dirty only for
    local iteration on this harness itself.
  - Each run gets its own isolated HOME (a fresh scratch directory) so no
    run can read state left behind by a previous run.
  - No new dependencies: stdlib only (argparse, json, re, statistics,
    subprocess).
"""

from __future__ import annotations

import argparse
import json
import os
import pathlib
import re
import statistics
import subprocess
import sys
import time

REPO_ROOT = pathlib.Path(__file__).parent.parent.parent
PERF_SCRIPTS_DIR = pathlib.Path(__file__).parent
PIPELINE_SCRIPT = PERF_SCRIPTS_DIR / "bench_pipeline_daemon.py"
LOAD_SCRIPT = PERF_SCRIPTS_DIR / "bench_load_harness.py"

DEFAULT_OUT_DIR = PERF_SCRIPTS_DIR / "calibration"


def _iso_now() -> str:
    t = time.gmtime()
    return f"{t.tm_year:04d}-{t.tm_mon:02d}-{t.tm_mday:02d}T{t.tm_hour:02d}:{t.tm_min:02d}:{t.tm_sec:02d}Z"


def _git_sha() -> str:
    out = subprocess.run(
        ["git", "rev-parse", "HEAD"], cwd=REPO_ROOT, capture_output=True, text=True, check=True
    )
    return out.stdout.strip()


def _git_dirty() -> bool:
    out = subprocess.run(
        ["git", "status", "--porcelain"], cwd=REPO_ROOT, capture_output=True, text=True, check=True
    )
    return bool(out.stdout.strip())


# ── suite: pipeline ───────────────────────────────────────────────────────────

_PIPELINE_MEAN_PAK_RE = re.compile(r"Mean Precision@K=([\d.]+)\s+floor=[\d.]+")
_PIPELINE_MEAN_VEC_RE = re.compile(r"Mean VectorOnly P@K=([\d.]+)\s+floor=[\d.]+")
_PIPELINE_MEAN_KW_RE = re.compile(r"Mean KeywordOnly P@K=([\d.]+)\s+floor=[\d.]+")
_PIPELINE_LATENCY_RE = re.compile(r"p50=(\d+)µs\s+p95=(\d+)µs\s+n_latencies=(\d+)")
_PIPELINE_QUERY_RE = re.compile(
    r"^\s{2}(.+?)\s{2,}P@K=([\d.]+)\s+vec=([\d.]+)\((?:PASS|FAIL)\)\s+"
    r"kw=([\d.]+)\((?:PASS|FAIL)\)\s+\((\d+) hits\)\s+p50=(\d+)µs\s+(?:PASS|FAIL)",
    re.MULTILINE,
)
_PIPELINE_GATE_RE = re.compile(r"^Gate: (PASS|FAIL)$", re.MULTILINE)


def _pipeline_build_cmd(run_dir: pathlib.Path, extra_args: list[str]) -> list[str]:
    return [sys.executable, str(PIPELINE_SCRIPT), *extra_args]


def _pipeline_extract(run_dir: pathlib.Path, proc: subprocess.CompletedProcess) -> dict[str, float]:
    stdout = proc.stdout
    metrics: dict[str, float] = {}

    m = _PIPELINE_MEAN_PAK_RE.search(stdout)
    if m:
        metrics["mean_precision_at_k"] = float(m.group(1))
    m = _PIPELINE_MEAN_VEC_RE.search(stdout)
    if m:
        metrics["mean_precision_vector_only"] = float(m.group(1))
    m = _PIPELINE_MEAN_KW_RE.search(stdout)
    if m:
        metrics["mean_precision_keyword_only"] = float(m.group(1))
    m = _PIPELINE_LATENCY_RE.search(stdout)
    if m:
        metrics["p50_us"] = float(m.group(1))
        metrics["p95_us"] = float(m.group(2))
        metrics["n_latencies"] = float(m.group(3))
    m = _PIPELINE_GATE_RE.search(stdout)
    if m:
        metrics["gate_pass"] = 1.0 if m.group(1) == "PASS" else 0.0

    for qm in _PIPELINE_QUERY_RE.finditer(stdout):
        topic = qm.group(1).strip().replace(" ", "_")
        metrics[f"query.{topic}.p_at_k"] = float(qm.group(2))
        metrics[f"query.{topic}.vec_p_at_k"] = float(qm.group(3))
        metrics[f"query.{topic}.kw_p_at_k"] = float(qm.group(4))
        metrics[f"query.{topic}.top_k_hits"] = float(qm.group(5))
        metrics[f"query.{topic}.p50_us"] = float(qm.group(6))

    return metrics


# ── suite: load ────────────────────────────────────────────────────────────

_LOAD_REPORT_FILENAME = "load_report.json"


def _load_build_cmd(run_dir: pathlib.Path, extra_args: list[str]) -> list[str]:
    report_path = run_dir / _LOAD_REPORT_FILENAME
    return [sys.executable, str(LOAD_SCRIPT), *extra_args, "--report", str(report_path)]


def _flatten_numeric(prefix: str, obj, out: dict[str, float]) -> None:
    if isinstance(obj, dict):
        for k, v in obj.items():
            _flatten_numeric(f"{prefix}.{k}" if prefix else str(k), v, out)
    elif isinstance(obj, bool):
        return
    elif isinstance(obj, (int, float)):
        out[prefix] = float(obj)
    # lists/strings (status enums, attribution detail, error text) are not
    # numeric samples; they remain visible in the per-run report.json instead.


def _load_extract(run_dir: pathlib.Path, proc: subprocess.CompletedProcess) -> dict[str, float]:
    report_path = run_dir / _LOAD_REPORT_FILENAME
    if report_path.exists():
        report = json.loads(report_path.read_text())
    else:
        # Fallback: the script also prints the same JSON report as its last
        # stdout write when --report could not be written for some reason.
        report = json.loads(proc.stdout.strip().splitlines()[-1])

    metrics: dict[str, float] = {}
    _flatten_numeric("", report.get("dimensions", {}), metrics)
    for k, v in report.get("op_counts", {}).items():
        metrics[f"op_counts.{k}"] = float(v)
    for k, v in report.get("op_error_counts", {}).items():
        metrics[f"op_error_counts.{k}"] = float(v)
    metrics["smoke_pass"] = 1.0 if report.get("smoke_result") == "PASS" else 0.0
    return metrics


# ── suite registry ────────────────────────────────────────────────────────────
#
# Each entry: {command argv builder, metric extractor, cheap CLI defaults,
# per-run subprocess timeout in seconds}. Add a suite by adding an entry
# here plus its _build_cmd / _extract pair above — nothing else in this
# file needs to change.

SUITES = {
    "pipeline": {
        "build_cmd": _pipeline_build_cmd,
        "extract": _pipeline_extract,
        "default_args": [],
        "timeout_s": 600,
    },
    "load": {
        "build_cmd": _load_build_cmd,
        "extract": _load_extract,
        # Cheap local defaults: bench-embedder (no GPU lock), small
        # worker/tenant fan-out. Override entirely via --suite-arg (each
        # --suite-arg token is appended after these, so pass the full
        # desired flag set, e.g. --suite-arg --workers --suite-arg 40, to
        # widen the run).
        "default_args": [
            "--mode", "bench",
            "--workers", "8",
            "--tenants", "4",
            "--ops-per-worker", "3",
            "--worker-timeout", "30",
            "--log-level", "warn",
        ],
        "timeout_s": 600,
    },
}


# ── runner ─────────────────────────────────────────────────────────────────────


def _run_once(suite_name: str, run_dir: pathlib.Path, extra_args: list[str]) -> tuple[subprocess.CompletedProcess, dict[str, float], float]:
    suite = SUITES[suite_name]
    argv = suite["build_cmd"](run_dir, extra_args)

    home_dir = run_dir / "home"
    home_dir.mkdir(parents=True, exist_ok=True)
    env = {**os.environ}
    env["HOME"] = str(home_dir)

    t0 = time.time()
    proc = subprocess.run(
        argv,
        cwd=str(REPO_ROOT),
        env=env,
        capture_output=True,
        text=True,
        timeout=suite["timeout_s"],
    )
    wall_s = time.time() - t0

    (run_dir / "stdout.log").write_text(proc.stdout)
    (run_dir / "stderr.log").write_text(proc.stderr)
    (run_dir / "argv.txt").write_text(" ".join(argv) + "\n")

    metrics = suite["extract"](run_dir, proc)
    metrics["_wall_s"] = wall_s
    metrics["_exit_code"] = float(proc.returncode)
    return proc, metrics, wall_s


def _advisory_floor(vals: list[float], mean: float, std: float) -> float:
    floor = mean - 3.0 * std
    # Ratios/precisions live in [0, 1] — clamp the suggested floor into that
    # range too. Counts/latencies are non-negative — never suggest a
    # negative floor.
    if min(vals) >= 0.0 and max(vals) <= 1.0:
        floor = min(floor, 1.0)
    return max(floor, 0.0)


def _build_profile(samples: dict[str, list[float]]) -> dict[str, dict]:
    profile = {}
    for name, vals in samples.items():
        n = len(vals)
        mean = statistics.fmean(vals)
        std = statistics.pstdev(vals) if n > 1 else 0.0
        cv = (std / mean) if mean else None
        profile[name] = {
            "n": n,
            "samples": vals,
            "mean": mean,
            "std": std,
            "min": min(vals),
            "max": max(vals),
            "cv": cv,
            "advisory_floor_mean_minus_3std": _advisory_floor(vals, mean, std),
        }
    return profile


def _render_markdown(payload: dict) -> str:
    lines = []
    lines.append(f"# Calibration: `{payload['suite']}` @ `{payload['git_sha_short']}`")
    lines.append("")
    lines.append(f"- suite: `{payload['suite']}`")
    lines.append(f"- git sha: `{payload['git_sha']}`")
    lines.append(f"- runs: {payload['runs']}")
    lines.append(f"- produced_at: {payload['produced_at']}")
    lines.append(f"- command: `{' '.join(payload['command_argv'])}`")
    lines.append("")
    lines.append(
        "All floors below are **ADVISORY** — `mean - 3*std`, clamped to `[0, 1]` for "
        "ratio-shaped metrics and to `>= 0` otherwise. This harness never sets a gate; "
        "it only measures same-SHA noise so a human can place one with a known margin."
    )
    lines.append("")
    lines.append("| metric | n | mean | std | cv | min | max | advisory floor (mean-3*std) |")
    lines.append("|---|---|---|---|---|---|---|---|")
    for name in sorted(payload["metrics"]):
        s = payload["metrics"][name]
        cv = f"{s['cv']:.4f}" if s["cv"] is not None else "n/a"
        lines.append(
            f"| {name} | {s['n']} | {s['mean']:.4g} | {s['std']:.4g} | {cv} | "
            f"{s['min']:.4g} | {s['max']:.4g} | {s['advisory_floor_mean_minus_3std']:.4g} |"
        )
    lines.append("")
    lines.append(
        "Note: for lower-is-better metrics (latencies, error/backpressure counts), "
        "`mean-3*std` as a *floor* is the wrong direction — read it as informational "
        "spread only. Whether a metric is higher-is-better, lower-is-better, or purely "
        "observational (e.g. `_wall_s`), and where to actually place a blocking gate "
        "relative to this noise, remains a human decision."
    )
    return "\n".join(lines) + "\n"


def main() -> int:
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    ap.add_argument("--suite", required=True, choices=sorted(SUITES), help="which bench suite to calibrate")
    ap.add_argument("--runs", type=int, default=5, help="number of same-SHA repetitions (default: 5)")
    ap.add_argument("--out", default=str(DEFAULT_OUT_DIR), help="output directory for the JSON/markdown profile and per-run artifacts")
    ap.add_argument(
        "--suite-arg",
        action="append",
        default=[],
        help="extra argv token appended to the suite's default args (repeatable, e.g. --suite-arg --workers --suite-arg 40)",
    )
    ap.add_argument(
        "--allow-dirty",
        action="store_true",
        help="skip the clean-worktree guard (defeats same-SHA calibration; local iteration on this harness only)",
    )
    args = ap.parse_args()

    if args.runs < 2:
        print("FATAL: --runs must be >= 2 (need at least 2 samples to compute std/CV).", file=sys.stderr)
        return 2

    if not args.allow_dirty and _git_dirty():
        print(
            "FATAL: git worktree is dirty. Same-SHA calibration measures run-to-run\n"
            "noise at a FIXED commit — a dirty tree conflates code-change variance with\n"
            "that noise. Commit or stash your changes, or pass --allow-dirty if you\n"
            "understand this harness's own development loop is the exception.",
            file=sys.stderr,
        )
        return 2

    sha = _git_sha()
    short_sha = sha[:8]

    out_dir = pathlib.Path(args.out)
    out_dir.mkdir(parents=True, exist_ok=True)
    runs_dir = out_dir / "runs" / f"{args.suite}_{short_sha}"
    runs_dir.mkdir(parents=True, exist_ok=True)

    suite = SUITES[args.suite]
    extra_args = [*suite["default_args"], *args.suite_arg]

    samples: dict[str, list[float]] = {}
    exit_codes = []
    print(f"[calibrate] suite={args.suite} runs={args.runs} sha={short_sha}", flush=True)
    for i in range(args.runs):
        run_dir = runs_dir / f"run{i + 1}"
        run_dir.mkdir(parents=True, exist_ok=True)
        print(f"[calibrate] run {i + 1}/{args.runs}...", flush=True)
        try:
            proc, metrics, wall_s = _run_once(args.suite, run_dir, extra_args)
        except subprocess.TimeoutExpired:
            print(f"[calibrate] run {i + 1} TIMED OUT after {suite['timeout_s']}s; aborting.", file=sys.stderr)
            return 1
        exit_codes.append(proc.returncode)
        for k, v in metrics.items():
            samples.setdefault(k, []).append(v)
        print(f"[calibrate]   done in {wall_s:.1f}s exit={proc.returncode}", flush=True)

    if not samples:
        print(
            "FATAL: no metrics extracted from any run — the suite's output format may\n"
            "have changed. Check the per-run stdout.log / load_report.json under:\n"
            f"  {runs_dir}",
            file=sys.stderr,
        )
        return 1

    profile = _build_profile(samples)
    payload = {
        "suite": args.suite,
        "git_sha": sha,
        "git_sha_short": short_sha,
        "runs": args.runs,
        "exit_codes": exit_codes,
        "produced_at": _iso_now(),
        "command_argv": suite["build_cmd"](runs_dir, extra_args),
        "run_artifacts_dir": str(runs_dir),
        "metrics": profile,
    }

    json_path = out_dir / f"calibration_{args.suite}_{short_sha}.json"
    json_path.write_text(json.dumps(payload, indent=2))
    md_path = out_dir / f"calibration_{args.suite}_{short_sha}.md"
    md_path.write_text(_render_markdown(payload))

    print(f"[calibrate] wrote {json_path}")
    print(f"[calibrate] wrote {md_path}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
