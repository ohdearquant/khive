#!/usr/bin/env python3
"""Same-SHA null-distribution calibration harness.

Runs a chosen bench suite K times at the CURRENT commit (unchanged) and
emits a variance profile per metric: samples, mean, std, min, max, CV, and
an ADVISORY-CALIBRATION-ONLY floor at mean-k(n)*std, where k(n) is the
one-sided normal tolerance factor sized from the run count n (see
_tolerance_factor_k). This harness never sets a gate threshold itself — it
measures run-to-run noise at a fixed SHA so a human can place a blocking
floor with a known noise budget behind it.

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
  bench-1m  drives `bash scripts/bench_1m.sh --ci-synthetic` - the exact
            command run by the blocking ann-ci-gate job in
            .github/workflows/bench-1m.yml. Hermetic: generates synthetic
            clustered fixtures (seed=42, no network, no SIFT-1M dataset)
            and runs the real khive-vamana vec_bench release binary.
            Extracts assertion checks (recall_at_10, beam_growth_exponent,
            speedup_vs_brute_force), per-N row metrics, and growth-exponent
            fits from the schema-versioned result JSON it writes.

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
import math
import os
import pathlib
import re
import signal
import statistics
import subprocess
import sys
import tempfile
import time
import unittest

REPO_ROOT = pathlib.Path(__file__).parent.parent.parent
PERF_SCRIPTS_DIR = pathlib.Path(__file__).parent
_BENCH1M_FIXTURE_PATH = PERF_SCRIPTS_DIR / "testdata" / "bench1m_result_fixture.json"
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


# ── suite: bench-1m ───────────────────────────────────────────────────────────
#
# Wraps the exact hermetic per-PR gate command run by .github/workflows/
# bench-1m.yml's `ann-ci-gate` job: `bash scripts/bench_1m.sh --ci-synthetic`.
# That mode generates deterministic synthetic clustered fixtures (seed=42,
# no network, no SIFT-1M dataset) via gen_synthetic_fvecs.py and runs the
# real khive-vamana vec_bench release binary against them - this is what
# actually blocks PRs, unlike the manual/self-hosted-only bench-1m-sift job
# (which requires a ~500MB SIFT-1M dataset at SIFT_DIR and cannot run
# standalone here; that job is intentionally NOT wrapped by this suite).
#
# bench_1m.sh writes its full result as a schema-versioned JSON file
# (BENCH_OUT/<dataset>.json) rather than only printing a table - the load
# suite's JSON-report style is reused here (more robust than regexing the
# println! summary table, which is free-form and not intended as a stable
# grammar). BENCH_OUT is pointed at this run's isolated run_dir so a run's
# JSON can never be read by, or leak into, another run.

_BENCH1M_DATASET = "SIFT-CI-synthetic"
_BENCH1M_EXPECTED_SCHEMA_VERSION = "1.0"  # crates/khive-vamana/examples/vec_bench.rs schema_version
_BENCH1M_EXPECTED_ROWS = 2  # --ci-synthetic fixes NS=10000,50000 (bench_1m.sh)
_BENCH1M_EXPECTED_ROW_NS = frozenset({10000, 50000})
_BENCH1M_ROW_NUMERIC_FIELDS = (
    "build_ms",
    "iso_recall_beam",
    "recall_at_10",
    "query_warm_p50_us",
    "query_warm_p95_us",
    "query_warm_p99_us",
    "query_warm_max_us",
    "bruteforce_p50_us",
    "speedup_vs_brute_force",
)
_BENCH1M_FITS_NUMERIC_FIELDS = (
    "beam_growth_exponent",
    "build_wallclock_exponent",
    "iso_recall_query_exponent_warm",
    "bruteforce_exponent",
)
# khive-vamana/ci-synthetic/clustered-128 target in perf/targets.toml. `scope`
# governs the shape of a check's "measured" field in the result JSON
# (evaluate_check in vec_bench.rs): "all_rows" serializes a Vec<f64> (one
# value per row), "fits"/"max_n" serialize a single f64. A probe result with
# an unexpected metric name or scope here is schema drift, not a metric to
# silently accept.
_BENCH1M_EXPECTED_CHECK_SCOPES = {
    "recall_at_10": "all_rows",
    "beam_growth_exponent": "fits",
    "speedup_vs_brute_force": "max_n",
}
# gate_pass (1) + 2 metrics/check (measured, pass) + row fields + fits fields.
_BENCH1M_EXPECTED_METRIC_COUNT = (
    1
    + len(_BENCH1M_EXPECTED_CHECK_SCOPES) * 2
    + _BENCH1M_EXPECTED_ROWS * len(_BENCH1M_ROW_NUMERIC_FIELDS)
    + len(_BENCH1M_FITS_NUMERIC_FIELDS)
)


def _bench1m_build_cmd(run_dir: pathlib.Path, extra_args: list[str]) -> list[str]:
    return ["bash", str(REPO_ROOT / "scripts" / "bench_1m.sh"), "--ci-synthetic", *extra_args]


def _bench1m_build_env(run_dir: pathlib.Path) -> dict[str, str]:
    return {"BENCH_OUT": str(run_dir / "bench-out")}


def _bench1m_extract(run_dir: pathlib.Path, proc: subprocess.CompletedProcess) -> dict[str, float]:
    json_path = run_dir / "bench-out" / f"{_BENCH1M_DATASET}.json"
    if not json_path.exists():
        raise SchemaError(
            f"bench-1m gate did not write the expected result JSON at {json_path}; "
            f"see {run_dir}/stdout.log"
        )
    result = json.loads(json_path.read_text())

    schema_version = result.get("schema_version")
    if schema_version != _BENCH1M_EXPECTED_SCHEMA_VERSION:
        raise SchemaError(
            f"bench-1m result schema_version={schema_version!r}, expected "
            f"{_BENCH1M_EXPECTED_SCHEMA_VERSION!r} - result JSON shape drift; see {json_path}"
        )

    metrics: dict[str, float] = {}

    assertions = result.get("assertions")
    if not isinstance(assertions, dict) or "overall" not in assertions:
        raise SchemaError(f"bench-1m result missing 'assertions.overall'; see {json_path}")
    metrics["gate_pass"] = 1.0 if assertions["overall"] == "PASS" else 0.0

    checks = assertions.get("checks")
    if not isinstance(checks, list) or len(checks) != len(_BENCH1M_EXPECTED_CHECK_SCOPES):
        raise SchemaError(
            f"expected exactly {len(_BENCH1M_EXPECTED_CHECK_SCOPES)} assertion checks in "
            f"bench-1m result (perf/targets.toml khive-vamana/ci-synthetic/clustered-128 "
            f"target), got {len(checks) if isinstance(checks, list) else type(checks).__name__}; "
            f"see {json_path}"
        )
    seen_check_metrics: set[str] = set()
    for check in checks:
        metric_name = check.get("metric")
        scope = check.get("scope")
        if not metric_name or metric_name in seen_check_metrics:
            raise SchemaError(
                f"bench-1m assertion check missing or duplicate 'metric' key: {check!r}; "
                f"see {json_path}"
            )
        expected_scope = _BENCH1M_EXPECTED_CHECK_SCOPES.get(metric_name)
        if expected_scope is None:
            raise SchemaError(
                f"bench-1m assertion check has unexpected metric {metric_name!r} "
                f"(expected one of {sorted(_BENCH1M_EXPECTED_CHECK_SCOPES)}); see {json_path}"
            )
        if scope != expected_scope:
            raise SchemaError(
                f"bench-1m assertion check {metric_name!r} has scope {scope!r}, "
                f"expected {expected_scope!r}; see {json_path}"
            )
        seen_check_metrics.add(metric_name)

        measured = check.get("measured")
        if scope == "all_rows":
            # evaluate_check's "all_rows" arm serializes a Vec<f64>, one
            # value per row - float(check["measured"]) crashes with
            # TypeError on this shape. Reduce to the single value that
            # actually binds the check's pass/fail (the worst case in the
            # operator's direction), so a single "measured" metric stays
            # comparable across scopes.
            if not isinstance(measured, list) or not measured or any(
                isinstance(v, bool) or not isinstance(v, (int, float)) for v in measured
            ):
                raise SchemaError(
                    f"bench-1m assertion check {metric_name!r} (scope=all_rows) expected a "
                    f"non-empty numeric array 'measured', got {measured!r}; see {json_path}"
                )
            operator = check.get("operator")
            if operator in (">=", ">"):
                value = min(measured)
            elif operator in ("<=", "<"):
                value = max(measured)
            else:
                raise SchemaError(
                    f"bench-1m assertion check {metric_name!r} (scope=all_rows) has "
                    f"unexpected operator {operator!r}; see {json_path}"
                )
        elif scope in ("fits", "max_n"):
            if isinstance(measured, bool) or not isinstance(measured, (int, float)):
                raise SchemaError(
                    f"bench-1m assertion check {metric_name!r} (scope={scope!r}) expected "
                    f"a numeric 'measured', got {measured!r}; see {json_path}"
                )
            value = measured
        else:
            raise SchemaError(
                f"bench-1m assertion check {metric_name!r} has unhandled scope {scope!r} "
                f"(expected all_rows/fits/max_n); see {json_path}"
            )
        metrics[f"assertion.{metric_name}.measured"] = float(value)
        metrics[f"assertion.{metric_name}.pass"] = 1.0 if check.get("result") == "PASS" else 0.0

    missing_checks = set(_BENCH1M_EXPECTED_CHECK_SCOPES) - seen_check_metrics
    if missing_checks:
        raise SchemaError(
            f"bench-1m result missing expected assertion checks: {sorted(missing_checks)}; "
            f"see {json_path}"
        )

    rows = result.get("rows")
    if not isinstance(rows, list) or len(rows) != _BENCH1M_EXPECTED_ROWS:
        raise SchemaError(
            f"expected exactly {_BENCH1M_EXPECTED_ROWS} rows in bench-1m result "
            f"(--ci-synthetic fixes NS=10000,50000), got "
            f"{len(rows) if isinstance(rows, list) else type(rows).__name__}; see {json_path}"
        )
    seen_ns: set[int] = set()
    for row in rows:
        n = row.get("n")
        if n is None or n in seen_ns:
            raise SchemaError(f"bench-1m row missing or duplicate 'n' key: {row!r}; see {json_path}")
        seen_ns.add(n)
        for field in _BENCH1M_ROW_NUMERIC_FIELDS:
            if field not in row:
                raise SchemaError(f"bench-1m row n={n} missing field {field!r}; see {json_path}")
            metrics[f"row.n{n}.{field}"] = float(row[field])
    if seen_ns != _BENCH1M_EXPECTED_ROW_NS:
        raise SchemaError(
            f"bench-1m rows have n={sorted(seen_ns)}, expected exactly "
            f"{sorted(_BENCH1M_EXPECTED_ROW_NS)} (--ci-synthetic fixes NS=10000,50000); "
            f"see {json_path}"
        )

    fits = result.get("fits")
    if not isinstance(fits, dict):
        raise SchemaError(f"bench-1m result missing 'fits' object; see {json_path}")
    for field in _BENCH1M_FITS_NUMERIC_FIELDS:
        if field not in fits:
            raise SchemaError(f"bench-1m result missing fits field {field!r}; see {json_path}")
        metrics[f"fits.{field}"] = float(fits[field])

    if len(metrics) != _BENCH1M_EXPECTED_METRIC_COUNT:
        raise SchemaError(
            f"expected exactly {_BENCH1M_EXPECTED_METRIC_COUNT} bench-1m metrics "
            f"(schema/cardinality drift), got {len(metrics)}; see {json_path}"
        )

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
    "bench-1m": {
        "build_cmd": _bench1m_build_cmd,
        "build_env": _bench1m_build_env,
        "extract": _bench1m_extract,
        "default_args": [],
        # cargo run --release builds+runs khive-vamana's vec_bench example
        # against synthetic 10K/50K-vector fixtures; a cold release build
        # can take several minutes on top of the bench itself.
        "timeout_s": 1800,
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
    orig_home = os.environ.get("HOME", "")
    env["HOME"] = str(home_dir)
    # Preserve toolchain resolution across the HOME swap above. rustup/cargo
    # (bench-1m's `cargo run --release`) and the pipeline/load suites' own
    # `~/.cargo/bin/kkernel` fallback resolve CARGO_HOME/RUSTUP_HOME/`~`
    # relative to HOME by default; isolating HOME for application-state
    # isolation must not also hide the toolchain from rustup ("rustup could
    # not choose a version of cargo"). Derive CARGO_HOME/RUSTUP_HOME from the
    # ORIGINAL HOME when the caller hasn't already pinned them explicitly, so
    # they keep pointing at the real toolchain install after HOME moves.
    if orig_home:
        env.setdefault("CARGO_HOME", str(pathlib.Path(orig_home) / ".cargo"))
        env.setdefault("RUSTUP_HOME", str(pathlib.Path(orig_home) / ".rustup"))
    build_env = suite.get("build_env")
    if build_env is not None:
        env.update(build_env(run_dir))

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


# ── n-sized one-sided normal tolerance factor (#829) ──────────────────────
#
# The advisory floor used to hardcode mean - 3*std, treating every run count
# the same regardless of how many same-SHA samples actually back the
# estimate. The bench-program calibration spec instead sizes the floor's
# tolerance factor k from n via the one-sided normal tolerance interval:
# with only n samples, mean and std are themselves estimates, and a small n
# needs a wider margin than a large n to make the same coverage/confidence
# claim. k(n) replaces the fixed 3.0 multiplier.
#
# Coverage P=0.99 (99%), confidence conf=0.95 (95%): the calibration spec's
# worked example pins k(10) ~= 3.98 for this coverage/confidence pair via
# the Wald-Wolfowitz approximation below - verified against an exact
# erf-based inverse normal (k(10) = 3.9400, within 0.05 of the pinned
# value).
#
# EXPLICIT RESOLUTION (round-1 review, #830): an earlier draft of this file's
# prose described the target as "99.9% coverage". That was a documentation
# error, not a second valid reading. Recomputing k(n) at 99.9%/95% gives
# k(5) = 7.5314 and k(10) = 5.1556 - neither matches issue #829's pinned
# k(10) ~= 3.98 acceptance target. At 99%/95% the same formula gives
# k(5) = 5.7504 and k(10) = 3.9400, which does match. 99% coverage / 95%
# confidence is therefore the correct reading and the only value this file
# implements; the constants below, every printed/rendered coverage label,
# and the JSON payload's tolerance_coverage field are all 0.99. If a future
# maintainer finds a spec revision that deliberately widens this to 99.9%,
# that is a spec change requiring a fresh k(10) pin and a matching update
# here - not a silent runtime toggle.
_TOLERANCE_COVERAGE = 0.99
_TOLERANCE_CONFIDENCE = 0.95


def _inv_norm_cdf(p: float) -> float:
    """Inverse standard-normal CDF (quantile function), stdlib only.

    Peter Acklam's rational approximation - relative error <= 1.15e-9 over
    (0, 1). No dependency on scipy/numpy.
    """
    if not 0.0 < p < 1.0:
        raise ValueError(f"p must be in (0, 1), got {p}")

    a = [
        -3.969683028665376e01, 2.209460984245205e02, -2.759285104469687e02,
        1.383577518672690e02, -3.066479806614716e01, 2.506628277459239e00,
    ]
    b = [
        -5.447609879822406e01, 1.615858368580409e02, -1.556989798598866e02,
        6.680131188771972e01, -1.328068155288572e01,
    ]
    c = [
        -7.784894002430293e-03, -3.223964580411365e-01, -2.400758277161838e00,
        -2.549732539343734e00, 4.374664141464968e00, 2.938163982698783e00,
    ]
    d = [
        7.784695709041462e-03, 3.224671290700398e-01, 2.445134137142996e00,
        3.754408661907416e00,
    ]
    p_low = 0.02425
    p_high = 1.0 - p_low

    if p < p_low:
        q = math.sqrt(-2.0 * math.log(p))
        return (((((c[0] * q + c[1]) * q + c[2]) * q + c[3]) * q + c[4]) * q + c[5]) / (
            (((d[0] * q + d[1]) * q + d[2]) * q + d[3]) * q + 1.0
        )
    if p <= p_high:
        q = p - 0.5
        r = q * q
        return (((((a[0] * r + a[1]) * r + a[2]) * r + a[3]) * r + a[4]) * r + a[5]) * q / (
            ((((b[0] * r + b[1]) * r + b[2]) * r + b[3]) * r + b[4]) * r + 1.0
        )
    q = math.sqrt(-2.0 * math.log(1.0 - p))
    return -(((((c[0] * q + c[1]) * q + c[2]) * q + c[3]) * q + c[4]) * q + c[5]) / (
        (((d[0] * q + d[1]) * q + d[2]) * q + d[3]) * q + 1.0
    )


class ToleranceDomainError(ValueError):
    """Raised when n is too small for the tolerance-factor approximation's valid domain."""


def _tolerance_factor_k(n: int, coverage: float = _TOLERANCE_COVERAGE, confidence: float = _TOLERANCE_CONFIDENCE) -> float:
    """One-sided normal tolerance factor k(n) (Wald-Wolfowitz approximation).

    k = (z_p + sqrt(z_p^2 - a*b)) / a
      a = 1 - z_conf^2 / (2*(n-1))
      b = z_p^2 - z_conf^2 / n

    z_p, z_conf are standard-normal quantiles at `coverage`/`confidence`
    (default: the module's pinned _TOLERANCE_COVERAGE/_TOLERANCE_CONFIDENCE).
    The coverage/confidence params exist so the self-check below can verify
    *other* readings against the spec's pinned k(10) without duplicating
    this formula - runtime callers never pass them. Requires n >= 2 (a is
    undefined at n=1).
    """
    if n < 2:
        raise ValueError(f"_tolerance_factor_k requires n >= 2, got {n}")
    z_p = _inv_norm_cdf(coverage)
    z_conf = _inv_norm_cdf(confidence)
    a = 1.0 - (z_conf**2) / (2.0 * (n - 1))
    if a <= 0.0:
        # The approximation's denominator only stays positive for
        # n > 1 + z_conf^2/2 (n >= 3 at confidence=0.95); below that the
        # formula is out of its valid domain and produces nonsense (a
        # negative or divide-by-near-zero result).
        raise ToleranceDomainError(
            f"_tolerance_factor_k(n={n}) is outside the approximation's valid domain "
            f"(a={a!r} <= 0); need a larger n"
        )
    b = z_p**2 - (z_conf**2) / n
    return (z_p + math.sqrt(z_p**2 - a * b)) / a


def _advisory_floor(
    vals: list[float], mean: float, std: float, n: int
) -> tuple[float, float] | tuple[None, None]:
    try:
        k = _tolerance_factor_k(n)
    except ToleranceDomainError:
        # n is too small (e.g. --runs 2) for the approximation's valid
        # domain - report no floor rather than crash the whole run; std/CV
        # are still meaningful at n=2, only the tolerance-sized floor is not.
        return None, None
    floor = mean - k * std
    # Ratios/precisions live in [0, 1] — clamp the suggested floor into that
    # range too. Counts/latencies are non-negative — never suggest a
    # negative floor.
    if min(vals) >= 0.0 and max(vals) <= 1.0:
        floor = min(floor, 1.0)
    return max(floor, 0.0), k


class ToleranceFactorSelfCheck(unittest.TestCase):
    """Self-check for _tolerance_factor_k. Run via: python3 -m unittest scripts.perf.bench_calibrate"""

    def test_k_10_matches_calibration_spec_pin(self) -> None:
        k10 = _tolerance_factor_k(10)
        self.assertLess(abs(k10 - 3.98), 0.05, f"k(10)={k10!r} not within 0.05 of the pinned 3.98")

    def test_coverage_is_99_not_99_9_percent(self) -> None:
        """Explicit resolution (#830 round-1): 99% coverage matches the #829
        pin; 99.9% does not. Locks in the module constants plus both sides
        of the discrepancy so a future edit can't silently flip this."""
        self.assertEqual(_TOLERANCE_COVERAGE, 0.99)
        self.assertEqual(_TOLERANCE_CONFIDENCE, 0.95)

        k5_99, k10_99 = _tolerance_factor_k(5, coverage=0.99), _tolerance_factor_k(10, coverage=0.99)
        self.assertAlmostEqual(k5_99, 5.7504, places=3)
        self.assertAlmostEqual(k10_99, 3.9400, places=3)

        k5_999, k10_999 = _tolerance_factor_k(5, coverage=0.999), _tolerance_factor_k(10, coverage=0.999)
        self.assertAlmostEqual(k5_999, 7.5314, places=3)
        self.assertAlmostEqual(k10_999, 5.1556, places=3)
        self.assertGreater(
            abs(k10_999 - 3.98), 0.05, "99.9% coverage's k(10) should NOT match the #829 pin"
        )

    def test_k_monotonically_decreasing_in_n(self) -> None:
        ks = [_tolerance_factor_k(n) for n in range(3, 201)]
        for i in range(len(ks) - 1):
            self.assertGreater(
                ks[i], ks[i + 1], f"k(n={i + 2})={ks[i]!r} <= k(n={i + 3})={ks[i + 1]!r} - not monotonically decreasing"
            )

    def test_k_requires_at_least_two_samples(self) -> None:
        with self.assertRaises(ValueError):
            _tolerance_factor_k(1)


class Bench1mExtractSelfCheck(unittest.TestCase):
    """Self-check for _bench1m_extract against the real vec_bench result shape
    (#830 round-1 finding: scope=all_rows serializes an array, and
    float(check["measured"]) crashed on it). Fixture captured from an actual
    `bash scripts/bench_1m.sh --ci-synthetic` run.

    Run via: python3 -m unittest scripts.perf.bench_calibrate
    """

    def _run_dir_with(self, tmp_path: pathlib.Path, payload: dict) -> pathlib.Path:
        bench_out = tmp_path / "bench-out"
        bench_out.mkdir()
        (bench_out / f"{_BENCH1M_DATASET}.json").write_text(json.dumps(payload))
        return tmp_path

    def _extract(self, tmp_path: pathlib.Path, payload: dict) -> dict[str, float]:
        run_dir = self._run_dir_with(tmp_path, payload)
        proc = subprocess.CompletedProcess([], 0, "", "")
        return _bench1m_extract(run_dir, proc)

    def test_real_result_shape_extracts_without_crashing(self) -> None:
        payload = json.loads(_BENCH1M_FIXTURE_PATH.read_text())
        recall_check = next(
            c for c in payload["assertions"]["checks"] if c["metric"] == "recall_at_10"
        )
        self.assertIsInstance(recall_check["measured"], list, "fixture must exercise the array shape")

        with tempfile.TemporaryDirectory() as td:
            metrics = self._extract(pathlib.Path(td), payload)

        self.assertEqual(len(metrics), _BENCH1M_EXPECTED_METRIC_COUNT)
        self.assertEqual(
            metrics["assertion.recall_at_10.measured"], min(recall_check["measured"])
        )
        self.assertEqual(metrics["assertion.recall_at_10.pass"], 1.0)
        self.assertEqual(metrics["row.n10000.recall_at_10"], payload["rows"][0]["recall_at_10"])

    def test_schema_version_drift_rejected(self) -> None:
        payload = json.loads(_BENCH1M_FIXTURE_PATH.read_text())
        payload["schema_version"] = "999.0"
        with tempfile.TemporaryDirectory() as td, self.assertRaises(SchemaError):
            self._extract(pathlib.Path(td), payload)

    def test_unexpected_assertion_metric_rejected(self) -> None:
        payload = json.loads(_BENCH1M_FIXTURE_PATH.read_text())
        payload["assertions"]["checks"][0]["metric"] = "totally_unexpected_metric"
        with tempfile.TemporaryDirectory() as td, self.assertRaises(SchemaError):
            self._extract(pathlib.Path(td), payload)

    def test_wrong_scope_for_known_metric_rejected(self) -> None:
        payload = json.loads(_BENCH1M_FIXTURE_PATH.read_text())
        payload["assertions"]["checks"][0]["scope"] = "fits"
        with tempfile.TemporaryDirectory() as td, self.assertRaises(SchemaError):
            self._extract(pathlib.Path(td), payload)

    def test_unexpected_row_n_rejected(self) -> None:
        payload = json.loads(_BENCH1M_FIXTURE_PATH.read_text())
        payload["rows"][0]["n"] = 12345
        with tempfile.TemporaryDirectory() as td, self.assertRaises(SchemaError):
            self._extract(pathlib.Path(td), payload)

    def test_malformed_all_rows_measured_rejected(self) -> None:
        payload = json.loads(_BENCH1M_FIXTURE_PATH.read_text())
        payload["assertions"]["checks"][0]["measured"] = 0.95  # scalar, not array
        with tempfile.TemporaryDirectory() as td, self.assertRaises(SchemaError):
            self._extract(pathlib.Path(td), payload)


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
        floor, k = _advisory_floor(vals, mean, std, n)
        profile[name] = {
            "n": n,
            "samples": vals,
            "mean": mean,
            "std": std,
            "min": min(vals),
            "max": max(vals),
            "cv": cv,
            "tolerance_k": k,
            "advisory_floor_calibration_only": floor,
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
        "All floors below are **ADVISORY-CALIBRATION-ONLY** — `mean - k(n)*std`, "
        "clamped to `[0, 1]` for ratio-shaped metrics and to `>= 0` otherwise. `k(n)` "
        "is the one-sided normal tolerance factor (Wald-Wolfowitz approximation) for "
        f"{_TOLERANCE_COVERAGE * 100:.1f}% coverage / {_TOLERANCE_CONFIDENCE * 100:.0f}% "
        "confidence, sized from the run count n so small same-SHA samples get a wider "
        "margin than large ones. This harness never sets a gate; it only measures "
        "same-SHA noise so a human can place one with a known margin."
    )
    lines.append("")
    lines.append("| metric | n | k(n) | mean | std | cv | min | max | advisory floor (calibration only) |")
    lines.append("|---|---|---|---|---|---|---|---|---|")
    for name in sorted(payload["metrics"]):
        s = payload["metrics"][name]
        cv = f"{s['cv']:.4f}" if s["cv"] is not None else "n/a"
        k_str = f"{s['tolerance_k']:.4f}" if s["tolerance_k"] is not None else "n/a"
        floor_str = (
            f"{s['advisory_floor_calibration_only']:.4g}"
            if s["advisory_floor_calibration_only"] is not None
            else "n/a (n too small)"
        )
        lines.append(
            f"| {name} | {s['n']} | {k_str} | {s['mean']:.4g} | {s['std']:.4g} | {cv} | "
            f"{s['min']:.4g} | {s['max']:.4g} | {floor_str} |"
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
    try:
        tolerance_k = _tolerance_factor_k(args.runs)
        print(
            f"[calibrate] tolerance factor k(n={args.runs})={tolerance_k:.4f} "
            f"(coverage={_TOLERANCE_COVERAGE * 100:.1f}%, "
            f"confidence={_TOLERANCE_CONFIDENCE * 100:.0f}%) - "
            "floors below are advisory-calibration-only.",
            flush=True,
        )
    except ToleranceDomainError as exc:
        tolerance_k = None
        print(
            f"[calibrate] tolerance factor k(n={args.runs}) undefined: {exc} - "
            "advisory floors omitted for this run (std/CV still reported).",
            flush=True,
        )
    payload = {
        "suite": args.suite,
        "git_sha": sha,
        "git_sha_short": short_sha,
        "runs": args.runs,
        "exit_codes": exit_codes,
        "produced_at": _iso_now(),
        "tolerance_k": tolerance_k,
        "tolerance_coverage": _TOLERANCE_COVERAGE,
        "tolerance_confidence": _TOLERANCE_CONFIDENCE,
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
