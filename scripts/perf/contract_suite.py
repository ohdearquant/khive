#!/usr/bin/env python3
"""Contract-suite runner for dimensions 1-3 (ingest, ANN query, recall@k).

Drives `khive-vamana`'s `vec_bench` release binary (scripts/bench_1m.sh's own
driver) once per invocation and derives the `ingest` and `ann_query`
contract-result-v1 dimensions from that single run, plus a separate exact
baseline (faiss-flat if an isolated venv is supplied, else a numpy vectorized
brute-force fallback) for the `recall_at_k` dimension. Emits exactly one
contract-result-v1 JSON document, validated in-process against
scripts/perf/schemas/contract-result-v1.json before it is written.

`ann_query` arms come from vec_bench's `--workers` concurrency arms (default
1,4,16 -- override with `--workers`); each arm is barrier-synchronized so its
p50/p95/p99 reflect that worker count actually querying concurrently.
`recall_at_k` also drives vec_bench's `--dump-topk` to get recall@1/@100, but
only emits the k=10 row in the schema (see `RECALL_ROW_KS` docstring for why:
no measured ANN latency exists at k=1/k=100 to pair with a required
speedup_vs_baseline); recall@1/@100 are still recorded in the document's
`notes` field.

This runner never computes a pass/fail verdict: it measures and records.
`calibration` is always `{"status": "uncalibrated"}` here -- calibrated runs
are produced separately by scripts/perf/bench_calibrate.py (K>=10, same SHA).

Usage:
    uv run python scripts/perf/contract_suite.py --selftest
    uv run python scripts/perf/contract_suite.py --dims ingest,ann_query,recall_at_k \\
        --scale 1000000 --sift-dir /path/to/sift1m --out /tmp/bench-s2-run \\
        --faiss-venv /private/tmp/khive-bench-s2-faiss --workers 1,4,16
"""

from __future__ import annotations

import argparse
import contextlib
import fcntl
import json
import os
import platform
import subprocess
import sys
import tempfile
import textwrap
from datetime import datetime, timezone
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
CRATES_DIR = REPO_ROOT / "crates"
SCHEMA_PATH = REPO_ROOT / "scripts/perf/schemas/contract-result-v1.json"
GEN_SYNTHETIC = REPO_ROOT / "scripts/perf/gen_synthetic_fvecs.py"
TARGETS_TOML = REPO_ROOT / "perf/targets.toml"
BENCH_WINDOW_LOCK = Path(os.environ.get("KHIVE_BENCH_LOCK", "/tmp/khive-bench-window.lock"))

ALL_DIMS = ("ingest", "ann_query", "recall_at_k")
VEC_BENCH_DEPENDENT = {"ingest", "ann_query", "recall_at_k"}

DEFAULT_WORKERS = "1,4,16"

# vec_bench's --dump-topk pass computes recall_at_1/recall_at_100 from an
# UNTIMED K=100 search at a larger beam (MAX_ISO_BEAM) than the TIMED K=10
# query arms (iso_recall_beam) -- it is a different search configuration, not
# a truncation of the timed arm's results. There is therefore no measured ANN
# latency at k=1 or k=100 to pair with those recall values, and
# contract-result-v1's recall_at_k row schema requires a numeric
# speedup_vs_baseline on every row (no recall-only row variant exists). This
# runner will not fabricate a k=1/k=100 latency by reusing the k=10 timing, so
# it emits the k=10 row only; recall@1/@100 are still measured (via
# --dump-topk) and reported in the document's `notes` field for visibility.
RECALL_ROW_KS = (10,)

BASELINE_WORKER = textwrap.dedent(
    r"""
    import json
    import sys
    import time

    import numpy as np


    def load_fvecs(path, limit=None):
        raw = np.fromfile(path, dtype="<i4")
        dim = int(raw[0])
        rec = dim + 1
        raw = raw.reshape(-1, rec)
        if limit is not None:
            raw = raw[:limit]
        vecs = raw[:, 1:].copy().view("<f4")
        return vecs, dim


    def main():
        base_path, query_path, n, n_queries, k, out_path = sys.argv[1:7]
        n = int(n)
        n_queries = int(n_queries)
        k = int(k)

        corpus, dim = load_fvecs(base_path, limit=n)
        queries, qdim = load_fvecs(query_path, limit=n_queries)
        assert dim == qdim, f"base dim={dim} != query dim={qdim}"

        kind = "vectorized-brute-force"
        try:
            import faiss

            kind = "faiss-flat"
            index = faiss.IndexFlatL2(dim)
            index.add(corpus.astype("float32"))
            index.search(queries[:1].astype("float32"), k)  # warmup
            times_us = []
            for i in range(queries.shape[0]):
                q = queries[i : i + 1].astype("float32")
                t0 = time.perf_counter()
                index.search(q, k)
                times_us.append((time.perf_counter() - t0) * 1e6)
        except ImportError:
            corpus_f = corpus.astype("float32")
            _ = ((corpus_f - queries[0]) ** 2).sum(axis=1)  # warmup
            times_us = []
            eff_k = min(k, corpus_f.shape[0])
            for i in range(queries.shape[0]):
                q = queries[i]
                t0 = time.perf_counter()
                d = ((corpus_f - q) ** 2).sum(axis=1)
                np.argpartition(d, eff_k - 1)[:eff_k]
                times_us.append((time.perf_counter() - t0) * 1e6)

        times_us.sort()
        p50 = times_us[min(len(times_us) - 1, int(len(times_us) * 0.50))]

        with open(out_path, "w") as f:
            json.dump({"kind": kind, "query_us_p50": p50, "n": n, "n_queries": n_queries}, f)


    if __name__ == "__main__":
        main()
    """
)


def log(msg: str) -> None:
    print(f"[contract-suite] {msg}", file=sys.stderr, flush=True)


def run(cmd: list[str], **kwargs) -> subprocess.CompletedProcess:
    log("+ " + " ".join(str(c) for c in cmd))
    return subprocess.run(cmd, **kwargs)


def parse_workers(spec: str) -> list[int]:
    out = []
    for part in spec.split(","):
        part = part.strip()
        if not part:
            raise ValueError(f"--workers contains an empty segment: {spec!r}")
        w = int(part)
        if w < 1:
            raise ValueError(f"--workers value must be >= 1, got {w!r}")
        out.append(w)
    if not out:
        raise ValueError("--workers must specify at least one worker count")
    return out


# ─── isolation evidence ───────────────────────────────────────────────────


def loadavg() -> list[float]:
    return list(os.getloadavg())


class BenchWindow:
    """Best-effort EXCLUSIVE, non-blocking acquisition of an optional exclusive
    advisory lock recording whether the measurement ran in an isolated bench
    window (path configurable via KHIVE_BENCH_LOCK). A failed acquisition is
    not an error -- it means concurrent build/bench activity was present and
    is recorded honestly as bench_window=False.
    """

    def __init__(self) -> None:
        self.held = False
        self._fd = None

    def __enter__(self) -> "BenchWindow":
        try:
            self._fd = os.open(str(BENCH_WINDOW_LOCK), os.O_RDWR | os.O_CREAT, 0o666)
            fcntl.flock(self._fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            self.held = True
        except (BlockingIOError, OSError) as e:
            log(f"bench-window: could not acquire EXCLUSIVE lock ({e}) -- recording bench_window=False")
            if self._fd is not None:
                os.close(self._fd)
                self._fd = None
        return self

    def __exit__(self, *exc) -> None:
        if self._fd is not None:
            fcntl.flock(self._fd, fcntl.LOCK_UN)
            os.close(self._fd)


def cargo_target_dir_isolated(cargo_target_dir: Path) -> bool:
    try:
        resolved = cargo_target_dir.resolve()
        resolved.relative_to(REPO_ROOT.resolve())
        return False  # inside the worktree -- not isolated
    except ValueError:
        return True


def host_info() -> dict:
    system = platform.system()
    cpu_count = os.cpu_count() or 1
    if system == "Darwin":
        machine = subprocess.run(
            ["sysctl", "-n", "machdep.cpu.brand_string"], capture_output=True, text=True
        ).stdout.strip() or platform.machine()
        mem_bytes = int(
            subprocess.run(["sysctl", "-n", "hw.memsize"], capture_output=True, text=True).stdout.strip()
            or 0
        )
        os_str = f"macos-{platform.machine()}"
    elif system == "Linux":
        machine = platform.machine()
        os_str = f"linux-{platform.machine()}"
        mem_bytes = 0
        try:
            with open("/proc/meminfo") as f:
                for line in f:
                    if line.startswith("MemTotal:"):
                        mem_bytes = int(line.split()[1]) * 1024
                        break
        except OSError:
            pass
    else:
        machine = platform.machine()
        os_str = system
        mem_bytes = 0
    return {
        "machine": machine or "unknown",
        "os": os_str,
        "cpu_count": cpu_count,
        "mem_bytes": mem_bytes or 1,
    }


def git_sha() -> str:
    return run(["git", "rev-parse", "HEAD"], cwd=REPO_ROOT, capture_output=True, text=True, check=True).stdout.strip()


def git_branch() -> str:
    return run(
        ["git", "rev-parse", "--abbrev-ref", "HEAD"], cwd=REPO_ROOT, capture_output=True, text=True, check=True
    ).stdout.strip()


def now_iso() -> str:
    return datetime.now(timezone.utc).isoformat()


# ─── dataset preparation ──────────────────────────────────────────────────


def prepare_dataset(scale: int, sift_dir: str | None, work_dir: Path) -> tuple[Path, Path, str, bool]:
    if sift_dir:
        base = Path(sift_dir) / "sift_base.fvecs"
        query = Path(sift_dir) / "sift_query.fvecs"
        missing = [str(p) for p in (base, query) if not p.exists()]
        if missing:
            raise FileNotFoundError(
                f"--sift-dir {sift_dir} was given but the following required file(s) are missing: "
                + ", ".join(missing)
            )
        log(f"using real SIFT-1M data at {sift_dir}")
        return base, query, "SIFT-1M", True

    synth_dir = work_dir / "synthetic-fvecs"
    synth_dir.mkdir(parents=True, exist_ok=True)
    log(f"generating synthetic clustered fixtures (n={scale}, seed=42) at {synth_dir}")
    run(
        [
            sys.executable,
            str(GEN_SYNTHETIC),
            "--out",
            str(synth_dir),
            "--n",
            str(scale),
            "--queries",
            "1000",
            "--dim",
            "128",
            "--clusters",
            "64",
            "--sigma",
            "0.08",
            "--seed",
            "42",
        ],
        check=True,
    )
    return synth_dir / "sift_base.fvecs", synth_dir / "sift_query.fvecs", "synthetic-clustered-128-seed42", False


# ─── vec_bench invocation ─────────────────────────────────────────────────


def run_vec_bench(
    base: Path,
    query: Path,
    n: int,
    dataset_name: str,
    out_json: Path,
    cargo_target_dir: Path,
    workers: list[int],
    dump_topk_path: Path | None,
) -> dict:
    out_json.parent.mkdir(parents=True, exist_ok=True)
    env = {**os.environ, "CARGO_TARGET_DIR": str(cargo_target_dir), "KHIVE_N_CAP": str(n)}
    cmd = [
        "cargo",
        "run",
        "--release",
        "-p",
        "khive-vamana",
        "--example",
        "vec_bench",
        "--",
        "--base",
        str(base),
        "--query",
        str(query),
        "--ns",
        str(n),
        "--dataset",
        dataset_name,
        "--targets",
        str(TARGETS_TOML),
        "--target-key",
        "contract-suite/unassessed",
        "--out",
        str(out_json),
        "--bank-run",
        str(out_json.parent / "bank-run"),
        "--workers",
        ",".join(str(w) for w in workers),
    ]
    if dump_topk_path is not None:
        cmd += ["--dump-topk", str(dump_topk_path)]
    proc = run(cmd, cwd=CRATES_DIR, env=env)
    rendered_cmd = " ".join(str(c) for c in cmd)
    if not out_json.exists():
        raise RuntimeError(
            f"vec_bench did not write its output JSON (exit={proc.returncode}); command: {rendered_cmd}"
        )
    with out_json.open() as f:
        data = json.load(f)
    if not data.get("rows"):
        raise RuntimeError("vec_bench output JSON has no rows")
    if proc.returncode != 0:
        overall = data.get("assertions", {}).get("overall")
        if overall != "SKIPPED":
            raise RuntimeError(
                f"vec_bench exited nonzero (exit={proc.returncode}, assertions.overall={overall!r}); "
                f"command: {rendered_cmd}"
            )
        log(
            f"vec_bench exited nonzero (exit={proc.returncode}) but assertions.overall=SKIPPED "
            "(no matching --target-key in targets.toml) -- accepting as unassessed, not a failure."
        )
    return data


# ─── recall_at_k baseline ─────────────────────────────────────────────────


def run_baseline(base: Path, query: Path, n: int, n_queries: int, k: int, faiss_venv: str | None, work_dir: Path) -> dict:
    worker_path = work_dir / "_baseline_worker.py"
    worker_path.write_text(BASELINE_WORKER)
    out_path = work_dir / "baseline.json"

    python_bin = sys.executable
    if faiss_venv:
        candidate = Path(faiss_venv) / "bin" / "python"
        if candidate.exists():
            python_bin = str(candidate)
        else:
            log(f"--faiss-venv {faiss_venv} has no bin/python -- falling back to {sys.executable} (numpy-only)")

    run(
        [python_bin, str(worker_path), str(base), str(query), str(n), str(n_queries), str(k), str(out_path)],
        check=True,
    )
    with out_path.open() as f:
        return json.load(f)


# ─── dimension builders ────────────────────────────────────────────────────


def build_ingest_dim(vb_row: dict, dataset_name: str) -> dict:
    n = vb_row["n"]
    build_s = vb_row["build_ms"] / 1000.0
    return {
        "dataset": dataset_name,
        "corpus_docs": n,
        "docs_per_s_embed_excluded": n / build_s,
        "index_build_wall_s": build_s,
    }


def build_ann_query_dim(vb_row: dict, dataset_name: str) -> dict:
    n = vb_row["n"]
    beam = vb_row["iso_recall_beam"]
    concurrency_arms = vb_row.get("concurrency_arms") or []
    if not concurrency_arms:
        raise RuntimeError(
            "vec_bench row has no concurrency_arms -- expected at least the workers=1 "
            "arm (vec_bench always defaults --workers to [1] when unset)"
        )
    arms = [
        {
            "workers": arm["workers"],
            "measured_recall_at_10": arm["recall_at_10"],
            "p50_us": arm["query_warm_p50_us"],
            "p95_us": arm["query_warm_p95_us"],
            "p99_us": arm["query_warm_p99_us"],
            "queries": arm["queries"],
            "beam": beam,
        }
        for arm in concurrency_arms
    ]
    log(f"ann_query: emitting {len(arms)} concurrency arm(s): workers={[a['workers'] for a in arms]}")
    return {"dataset": dataset_name, "n_vectors": n, "arms": arms}


def build_recall_at_k_dim(vb_row: dict, baselines_by_k: dict[int, dict], dataset_name: str) -> dict:
    log(
        "recall_at_k: emitting the k=10 row only. vec_bench also measures recall@1/@100 "
        "via --dump-topk, but that is an UNTIMED separate K=100 search at a larger beam "
        "than the timed K=10 arms -- there is no matching ANN latency for k=1/k=100, and "
        "contract-result-v1 requires a numeric speedup_vs_baseline on every row (no "
        "recall-only row variant). Those two recall values are recorded in the top-level "
        "`notes` field instead of being paired with a fabricated latency."
    )
    rows = []
    for k in RECALL_ROW_KS:
        baseline = baselines_by_k[k]
        rows.append(
            {
                "k": k,
                "recall": vb_row["recall_at_10"],
                "speedup_vs_baseline": baseline["query_us_p50"] / vb_row["query_warm_p50_us"],
            }
        )
    primary = baselines_by_k[RECALL_ROW_KS[0]]
    return {
        "dataset": dataset_name,
        "baseline": {"kind": primary["kind"], "query_us_p50": primary["query_us_p50"]},
        "rows": rows,
    }


def recall_dump_note(vb_row: dict) -> str | None:
    r1 = vb_row.get("recall_at_1")
    r100 = vb_row.get("recall_at_100")
    if r1 is None and r100 is None:
        return None
    dump_path = vb_row.get("topk_dump_path")
    return (
        f"diagnostic (not a contract-result row): recall_at_1={r1!r}, recall_at_100={r100!r} "
        f"measured by vec_bench --dump-topk (untimed K=100 pass, dump written to "
        f"{dump_path!r}); no matching ANN latency exists for these k, so no "
        "speedup_vs_baseline could be computed -- see recall_at_k dimension for the k=10 "
        "row, which is the only one with a measured ANN latency."
    )


# ─── main ──────────────────────────────────────────────────────────────────


@contextlib.contextmanager
def work_dir_context(out_dir: Path, keep_work: bool):
    """Work material (baseline worker script, vec_bench raw JSON, bank-run
    dir, synthetic corpus) is scratch, not the contract-result document.
    Default: an actual temp directory, removed on exit. --keep-work opts
    into retaining it as out_dir/_work for debugging.
    """
    if keep_work:
        d = out_dir / "_work"
        d.mkdir(parents=True, exist_ok=True)
        yield d
    else:
        with tempfile.TemporaryDirectory(prefix="khive-bench-s2-work-") as td:
            yield Path(td)


def parse_args() -> argparse.Namespace:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--dims", default=",".join(ALL_DIMS), help="comma-separated subset of: " + ",".join(ALL_DIMS))
    p.add_argument("--scale", type=int, default=10000, help="vector count (default: 10000)")
    p.add_argument("--sift-dir", default=None, help="directory containing sift_base.fvecs + sift_query.fvecs")
    p.add_argument("--out", default=None, help="output run directory (contract-result.json written inside)")
    p.add_argument("--faiss-venv", default=None, help="path to an isolated venv with faiss-cpu installed")
    p.add_argument(
        "--workers",
        default=DEFAULT_WORKERS,
        help=f"comma-separated concurrency arms passed to vec_bench --workers (default: {DEFAULT_WORKERS})",
    )
    p.add_argument("--selftest", action="store_true", help="synthetic 10K smoke run of all dims + schema validation")
    p.add_argument(
        "--keep-work",
        action="store_true",
        help="retain work artifacts (raw vec_bench JSON, bank-run dir, synthetic corpus) under --out/_work",
    )
    return p.parse_args()


def main() -> int:
    args = parse_args()

    if args.selftest:
        dims = list(ALL_DIMS)
        scale = 10000
        sift_dir = None
        out_dir = Path(tempfile.mkdtemp(prefix="khive-bench-s2-selftest-"))
        workers = [1, 2]
    else:
        dims = [d.strip() for d in args.dims.split(",") if d.strip()]
        unknown = set(dims) - set(ALL_DIMS)
        if unknown:
            log(f"ERROR: unknown dimension(s): {sorted(unknown)}")
            return 3
        scale = args.scale
        sift_dir = args.sift_dir
        if not args.out:
            log("ERROR: --out is required unless --selftest is given")
            return 3
        out_dir = Path(args.out)
        out_dir.mkdir(parents=True, exist_ok=True)
        try:
            workers = parse_workers(args.workers)
        except ValueError as e:
            log(f"ERROR: {e}")
            return 3

    cargo_target_dir = Path(os.environ.get("CARGO_TARGET_DIR", "/private/tmp/khive-bench-s2-target"))

    started_at = now_iso()
    lv_before = loadavg()

    dimensions: dict = {}
    bench_window_held = False

    with work_dir_context(out_dir, args.keep_work) as work_dir:
        base, query, dataset_name, is_real_sift = prepare_dataset(scale, sift_dir, work_dir)

        with BenchWindow() as bw:
            bench_window_held = bw.held

            vb_data = None
            vb_row = None
            if VEC_BENCH_DEPENDENT & set(dims):
                vb_json_path = work_dir / "vec_bench_out.json"
                dump_topk_path = work_dir / "topk-dump.jsonl" if "recall_at_k" in dims else None
                vb_data = run_vec_bench(
                    base, query, scale, dataset_name, vb_json_path, cargo_target_dir, workers, dump_topk_path
                )
                vb_row = next(r for r in vb_data["rows"] if r["n"] == scale)

            if "ingest" in dims:
                dimensions["ingest"] = build_ingest_dim(vb_row, dataset_name)

            if "ann_query" in dims:
                dimensions["ann_query"] = build_ann_query_dim(vb_row, dataset_name)

            if "recall_at_k" in dims:
                n_queries = vb_data.get("config", {}).get("n_gt_queries", min(1000, scale))
                baselines_by_k = {
                    k: run_baseline(base, query, scale, n_queries, k, args.faiss_venv, work_dir)
                    for k in RECALL_ROW_KS
                }
                dimensions["recall_at_k"] = build_recall_at_k_dim(vb_row, baselines_by_k, dataset_name)

    lv_after = loadavg()
    finished_at = now_iso()

    if not bench_window_held:
        log(
            f"isolation: did NOT hold the exclusive {BENCH_WINDOW_LOCK} lock during "
            "measurement (concurrent build/bench activity was present or the lock was "
            "unavailable) -- recording bench_window=False honestly."
        )

    notes = [
        "ledger_rows_appended=0: this runner does not append perf/ledger.csv rows "
        "in this slice (that ledger's schema is scale-proof-specific and unrelated "
        "to contract-result-v1; banking is a separate follow-up)."
    ]
    if vb_row is not None:
        dump_note = recall_dump_note(vb_row)
        if dump_note:
            notes.append(dump_note)

    document = {
        "schema_version": 1,
        "suite": "local-engine-contract",
        "sha": git_sha(),
        "branch": git_branch(),
        "started_at": started_at,
        "finished_at": finished_at,
        "host": host_info(),
        "isolation": {
            "bench_window": bench_window_held,
            "loadavg_before": lv_before,
            "loadavg_after": lv_after,
            "cargo_target_dir_isolated": cargo_target_dir_isolated(cargo_target_dir),
        },
        "calibration": {"status": "uncalibrated"},
        "dimensions": dimensions,
        "ledger_rows_appended": 0,
        "notes": " ".join(notes),
    }

    import jsonschema

    with SCHEMA_PATH.open() as f:
        schema = json.load(f)
    try:
        jsonschema.validate(document, schema)
    except jsonschema.ValidationError as e:
        log(f"ERROR: emitted document failed schema validation: {e}")
        return 1

    result_path = out_dir / "contract-result.json"
    with result_path.open("w") as f:
        json.dump(document, f, indent=2)
        f.write("\n")

    log(f"wrote {result_path}")
    log(f"dimensions measured: {sorted(dimensions.keys())}")
    log(f"dimensions NOT measured (omitted, not zero): {sorted(set(ALL_DIMS) - set(dimensions.keys()))}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
