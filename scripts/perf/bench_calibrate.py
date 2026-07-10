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
import signal
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


def _revalidate_same_sha(expected_sha: str, allow_dirty: bool, when: str) -> str | None:
    """Recheck HEAD sha + worktree cleanliness. Returns an error message, or None if clean."""
    sha = _git_sha()
    if sha != expected_sha:
        return (
            f"HEAD sha changed {when} (expected {expected_sha}, now {sha}) - "
            "same-SHA calibration invalidated."
        )
    if not allow_dirty and _git_dirty():
        return f"git worktree became dirty {when} - same-SHA calibration invalidated."
    return None


class ChildFailure(RuntimeError):
    """Raised when a suite subprocess exits nonzero. Fails the whole calibration closed."""

    def __init__(self, run_dir: pathlib.Path, returncode: int):
        self.run_dir = run_dir
        self.returncode = returncode
        super().__init__(
            f"child process exited {returncode} (run dir: {run_dir}) - aborting before "
            "metric extraction; no profile written."
        )


class SchemaError(RuntimeError):
    """Raised when a suite's stdout/report does not match the required metric schema."""


def _kill_process_tree(pgid: int, proc: subprocess.Popen) -> None:
    """Terminate the whole process group `pgid` launched for `proc` (start_new_session=True).

    subprocess timeouts only kill the direct child; suites spawn a kkernel/daemon
    descendant that would otherwise survive the timeout and keep holding the DB/socket.
    `pgid` must be captured by the caller right after Popen (while the direct child is
    still guaranteed alive) - deriving it from `proc.pid` here would race: once the
    direct child has exited (even just to zombie state), os.getpgid(proc.pid) can raise
    ProcessLookupError and skip the kill entirely, letting a SIGTERM-resistant
    descendant survive. SIGKILL is always sent to the saved pgid after the grace
    period, even if the direct child already exited from SIGTERM.
    """
    try:
        os.killpg(pgid, signal.SIGTERM)
    except ProcessLookupError:
        return
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        pass
    try:
        os.killpg(pgid, signal.SIGKILL)
    except ProcessLookupError:
        pass


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

# QUERIES in bench_pipeline_daemon.py is a fixed 15-row list; each row yields 5
# per-query metrics (p_at_k, vec_p_at_k, kw_p_at_k, top_k_hits, p50_us) plus 7
# summary/gate metrics (mean p@k x3, p50/p95/n_latencies, gate_pass) = 82 total.
_PIPELINE_EXPECTED_QUERY_ROWS = 15
_PIPELINE_METRICS_PER_QUERY = 5
_PIPELINE_SUMMARY_METRIC_COUNT = 7
_PIPELINE_EXPECTED_METRIC_COUNT = (
    _PIPELINE_EXPECTED_QUERY_ROWS * _PIPELINE_METRICS_PER_QUERY + _PIPELINE_SUMMARY_METRIC_COUNT
)


def _pipeline_build_cmd(run_dir: pathlib.Path, extra_args: list[str]) -> list[str]:
    return [sys.executable, str(PIPELINE_SCRIPT), *extra_args]


def _pipeline_extract(run_dir: pathlib.Path, proc: subprocess.CompletedProcess) -> dict[str, float]:
    stdout = proc.stdout
    metrics: dict[str, float] = {}

    def _require(m: re.Match | None, desc: str) -> re.Match:
        if m is None:
            raise SchemaError(
                f"pipeline stdout missing expected {desc}; see {run_dir}/stdout.log"
            )
        return m

    m = _require(_PIPELINE_MEAN_PAK_RE.search(stdout), "mean Precision@K summary line")
    metrics["mean_precision_at_k"] = float(m.group(1))
    m = _require(_PIPELINE_MEAN_VEC_RE.search(stdout), "mean VectorOnly P@K summary line")
    metrics["mean_precision_vector_only"] = float(m.group(1))
    m = _require(_PIPELINE_MEAN_KW_RE.search(stdout), "mean KeywordOnly P@K summary line")
    metrics["mean_precision_keyword_only"] = float(m.group(1))
    m = _require(_PIPELINE_LATENCY_RE.search(stdout), "p50/p95/n_latencies summary line")
    metrics["p50_us"] = float(m.group(1))
    metrics["p95_us"] = float(m.group(2))
    metrics["n_latencies"] = float(m.group(3))
    m = _require(_PIPELINE_GATE_RE.search(stdout), "'Gate: PASS/FAIL' line")
    metrics["gate_pass"] = 1.0 if m.group(1) == "PASS" else 0.0

    query_rows = list(_PIPELINE_QUERY_RE.finditer(stdout))
    if len(query_rows) != _PIPELINE_EXPECTED_QUERY_ROWS:
        raise SchemaError(
            f"expected {_PIPELINE_EXPECTED_QUERY_ROWS} per-query rows in pipeline stdout, "
            f"found {len(query_rows)}; see {run_dir}/stdout.log"
        )

    seen_topics: set[str] = set()
    for qm in query_rows:
        topic = qm.group(1).strip().replace(" ", "_")
        if topic in seen_topics:
            raise SchemaError(
                f"duplicate per-query row for topic {topic!r} in pipeline stdout; "
                f"see {run_dir}/stdout.log"
            )
        seen_topics.add(topic)
        metrics[f"query.{topic}.p_at_k"] = float(qm.group(2))
        metrics[f"query.{topic}.vec_p_at_k"] = float(qm.group(3))
        metrics[f"query.{topic}.kw_p_at_k"] = float(qm.group(4))
        metrics[f"query.{topic}.top_k_hits"] = float(qm.group(5))
        metrics[f"query.{topic}.p50_us"] = float(qm.group(6))

    if len(metrics) != _PIPELINE_EXPECTED_METRIC_COUNT:
        raise SchemaError(
            f"expected exactly {_PIPELINE_EXPECTED_METRIC_COUNT} pipeline metrics "
            f"({_PIPELINE_EXPECTED_QUERY_ROWS} query rows x {_PIPELINE_METRICS_PER_QUERY} + "
            f"{_PIPELINE_SUMMARY_METRIC_COUNT} summary), got {len(metrics)}; "
            f"see {run_dir}/stdout.log"
        )

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
        try:
            report = json.loads(proc.stdout.strip().splitlines()[-1])
        except (IndexError, json.JSONDecodeError) as exc:
            raise SchemaError(
                f"no {_LOAD_REPORT_FILENAME} written and stdout does not end in a JSON "
                f"report; see {run_dir}/stdout.log"
            ) from exc

    if "dimensions" not in report:
        raise SchemaError(
            f"load report missing required 'dimensions' key; see {report_path}"
        )

    metrics: dict[str, float] = {}
    _flatten_numeric("", report.get("dimensions", {}), metrics)
    for k, v in report.get("op_counts", {}).items():
        metrics[f"op_counts.{k}"] = float(v)
    for k, v in report.get("op_error_counts", {}).items():
        metrics[f"op_error_counts.{k}"] = float(v)

    # Validate the extractor actually pulled real suite metrics BEFORE
    # merging in bookkeeping keys (smoke_pass) - otherwise an empty
    # dimensions/op_counts/op_error_counts tree still yields a non-empty
    # dict and the no-metrics guard downstream never fires.
    if not metrics:
        raise SchemaError(
            f"no numeric metrics extracted from load report dimensions/op_counts/"
            f"op_error_counts; see {report_path}"
        )

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
        # desired flag set, e.g. --suite-arg=--workers --suite-arg=40, to
        # widen the run). Use the `=` form for dash-prefixed values -
        # argparse's append action cannot tell a space-separated
        # `--suite-arg --workers` from a new top-level flag.
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

    # run_dir is created fresh (exist_ok=False) by the caller for every
    # invocation - home_dir must be too, so no run can read state (including
    # a stale load_report.json) left behind by a prior run.
    home_dir = run_dir / "home"
    home_dir.mkdir(parents=True, exist_ok=False)
    env = {**os.environ}
    env["HOME"] = str(home_dir)

    t0 = time.time()
    # start_new_session=True puts the child in its own process group so a
    # timeout can kill the whole tree (kkernel/daemon descendants included),
    # not just the direct child subprocess.run would otherwise leave orphaned.
    proc = subprocess.Popen(
        argv,
        cwd=str(REPO_ROOT),
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        start_new_session=True,
    )
    # Captured immediately - the direct child is guaranteed alive right
    # after Popen returns, so this cannot race a fast-exiting child the way
    # a lazy os.getpgid(proc.pid) inside the timeout handler would.
    pgid = os.getpgid(proc.pid)
    try:
        stdout, stderr = proc.communicate(timeout=suite["timeout_s"])
    except subprocess.TimeoutExpired:
        _kill_process_tree(pgid, proc)
        # Bounded: _kill_process_tree already SIGKILLed the whole process
        # group, so the pipes should close promptly; never block forever on
        # a descendant that somehow still holds a write end open.
        try:
            stdout, stderr = proc.communicate(timeout=10)
        except subprocess.TimeoutExpired:
            stdout, stderr = "", ""
        wall_s = time.time() - t0
        (run_dir / "stdout.log").write_text(stdout or "")
        (run_dir / "stderr.log").write_text(stderr or "")
        (run_dir / "argv.txt").write_text(" ".join(argv) + "\n")
        raise
    wall_s = time.time() - t0

    (run_dir / "stdout.log").write_text(stdout)
    (run_dir / "stderr.log").write_text(stderr)
    (run_dir / "argv.txt").write_text(" ".join(argv) + "\n")

    # FAIL CLOSED: a nonzero child exit aborts before any metric extraction.
    # No profile may be written from a run whose child process failed.
    if proc.returncode != 0:
        raise ChildFailure(run_dir, proc.returncode)

    cp = subprocess.CompletedProcess(argv, proc.returncode, stdout, stderr)
    metrics = suite["extract"](run_dir, cp)
    # The extractor itself must have returned real suite metrics - raise
    # loudly here, before merging in the _wall_s bookkeeping key, so a
    # bookkeeping-only profile can never slip past the no-metrics guard.
    if not metrics:
        raise SchemaError(
            f"suite '{suite_name}' extractor returned no metrics; see {run_dir}/stdout.log"
        )
    metrics["_wall_s"] = wall_s
    return cp, metrics, wall_s


def _advisory_floor(vals: list[float], mean: float, std: float) -> float:
    floor = mean - 3.0 * std
    # Ratios/precisions live in [0, 1] — clamp the suggested floor into that
    # range too. Counts/latencies are non-negative — never suggest a
    # negative floor.
    if min(vals) >= 0.0 and max(vals) <= 1.0:
        floor = min(floor, 1.0)
    return max(floor, 0.0)


_CV_NEAR_ZERO_EPS = 1e-9


def _build_profile(samples: dict[str, list[float]]) -> dict[str, dict]:
    profile = {}
    for name, vals in samples.items():
        n = len(vals)
        mean = statistics.fmean(vals)
        # Sample std (n-1 denominator, statistics.stdev) - a population std
        # (statistics.pstdev) understates spread for the small same-SHA
        # sample sizes (--runs) this harness is built around.
        std = statistics.stdev(vals) if n > 1 else 0.0
        cv = (std / mean) if abs(mean) > _CV_NEAR_ZERO_EPS else None
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
        help=(
            "extra argv token appended to the suite's default args (repeatable). "
            "For dash-prefixed values use the '=' form so argparse doesn't mistake "
            "the value for a new flag, e.g. --suite-arg=--workers --suite-arg=40"
        ),
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

    # Fresh isolation per invocation: the run dir carries a nanosecond
    # timestamp + pid suffix so two invocations at the same SHA never share
    # (and can never silently read) each other's run artifacts. FAIL rather
    # than reuse if this exact dir is somehow already there.
    invocation_tag = f"{time.time_ns()}_{os.getpid()}"
    runs_dir = out_dir / "runs" / f"{args.suite}_{short_sha}_{invocation_tag}"
    if runs_dir.exists():
        print(f"FATAL: run directory {runs_dir} already exists; refusing to reuse it.", file=sys.stderr)
        return 2
    runs_dir.mkdir(parents=True, exist_ok=False)

    suite = SUITES[args.suite]
    extra_args = [*suite["default_args"], *args.suite_arg]

    samples: dict[str, list[float]] = {}
    exit_codes = []
    first_key_set: set[str] | None = None
    print(f"[calibrate] suite={args.suite} runs={args.runs} sha={short_sha}", flush=True)
    for i in range(args.runs):
        err = _revalidate_same_sha(sha, args.allow_dirty, f"before run {i + 1}")
        if err:
            print(f"FATAL: {err} No profile written.", file=sys.stderr)
            return 2

        run_dir = runs_dir / f"run{i + 1}"
        if run_dir.exists():
            print(f"FATAL: run directory {run_dir} already exists; refusing to reuse it.", file=sys.stderr)
            return 2
        run_dir.mkdir(parents=True, exist_ok=False)

        print(f"[calibrate] run {i + 1}/{args.runs}...", flush=True)
        try:
            proc, metrics, wall_s = _run_once(args.suite, run_dir, extra_args)
        except subprocess.TimeoutExpired:
            print(
                f"[calibrate] run {i + 1} TIMED OUT after {suite['timeout_s']}s "
                "(process group killed); aborting. No profile written.",
                file=sys.stderr,
            )
            return 1
        except ChildFailure as exc:
            print(f"[calibrate] FATAL on run {i + 1}: {exc}", file=sys.stderr)
            return 1
        except SchemaError as exc:
            print(
                f"[calibrate] FATAL: schema/cardinality check failed on run {i + 1}: {exc}\n"
                "No profile written.",
                file=sys.stderr,
            )
            return 1

        key_set = set(metrics)
        if first_key_set is None:
            first_key_set = key_set
        elif key_set != first_key_set:
            added = sorted(key_set - first_key_set)
            removed = sorted(first_key_set - key_set)
            print(
                f"[calibrate] FATAL: metric key-set drift on run {i + 1} vs run 1 "
                f"(added={added} removed={removed}); aborting. No profile written.",
                file=sys.stderr,
            )
            return 1

        err = _revalidate_same_sha(sha, args.allow_dirty, f"after run {i + 1}")
        if err:
            print(f"FATAL: {err} No profile written.", file=sys.stderr)
            return 2

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
