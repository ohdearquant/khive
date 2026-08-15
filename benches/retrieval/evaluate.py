#!/usr/bin/env python3
"""Retrieval regression-gate runner for the committed eval harness.

Seeds a fresh, isolated scratch khive database with the deterministic 400-note
corpus from generate_corpus.py, runs the 40 graded queries in queries.jsonl
through a named retrieval condition, and reports nDCG@10 / Recall@100 /
TargetRecall@100 / MRR@10, overall and per query_class.

Never touches a production database: HOME and KHIVE_DB are both redirected
into a fresh scratch directory for the duration of the run, and the script
refuses to start if an inherited KHIVE_DB already points at an existing file
outside that scratch directory.

Usage:
    uv run python evaluate.py --out results/A_fused_direct.jsonl
    uv run python evaluate.py --check-gold
"""

from __future__ import annotations

import argparse
import json
import math
import os
import re
import sqlite3
import subprocess
import sys
import tempfile
from collections import defaultdict
from datetime import datetime
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
import generate_corpus

HERE = Path(__file__).parent
DEFAULT_QUERIES = HERE / "queries.jsonl"
DEFAULT_GOLD = HERE / "gold" / "A_fused_direct.json"
SCRATCH_MARKER = "khive-eval-retrieval-"
CANDIDATE_POOL = 100

# Pinned so gold stays stable even if the runtime's built-in default embedding
# model changes; this is the current default (lattice_embed::EmbeddingModel::
# AllMiniLmL6V2 -> "all-minilm-l6-v2", crates/khive-runtime/src/config.rs).
PINNED_EMBEDDING_MODEL = "all-minilm-l6-v2"

# Explicit, non-default recall scoring weights applied to every condition via
# memory.recall's `config` arg (RecallConfig.scoring.weights): the temporal
# term is zeroed so gold is clock-hermetic (see README "Determinism"
# section). relevance/salience keep the product defaults (0.7/0.2) so the
# weight sum stays positive and stable.
#
# default_token_budget is pinned to the server-side DoS cap (MAX_TOKEN_BUDGET,
# crates/khive-pack-memory/src/scoring.rs) rather than the product default
# (4000): the response token budget is applied after top_k ranking
# (crates/khive-pack-memory/src/handlers/recall.rs handle_recall, the
# `budget_cutoff` loop) and can truncate a 100-hit pool below what a
# Recall@100 label requires. Pinning to the fixed server-side maximum keeps
# the corpus untruncated (worst case ~31k chars for the 100 longest notes in
# this harness's corpus vs. a 16000*4=64000-char budget at this setting)
# without deriving the number from actual corpus content, which would make
# gold non-hermetic.
HERMETIC_RECALL_CONFIG = {
    "scoring": {
        "weights": {"relevance": 0.7, "salience": 0.2, "temporal": 0.0},
        "default_token_budget": 16000,
    }
}

# Conditions table: name -> extra memory.recall args layered on the shared
# base (query/top_k/include_breakdown/config). New legs (sparse fusion,
# reranker) plug in here as additional named entries.
CONDITIONS = {
    "A_fused_direct": {},
}


def refuse_unsafe_db_env() -> None:
    env_db = os.environ.get("KHIVE_DB")
    if not env_db:
        return
    resolved = Path(env_db).expanduser()
    if resolved.exists() and SCRATCH_MARKER not in str(resolved):
        raise SystemExit(
            f"refusing to run: KHIVE_DB={resolved} already exists and is outside this "
            "harness's scratch directory. This harness never reads or writes a "
            "pre-existing or production database. Unset KHIVE_DB and re-run."
        )


def _ambient_symlink(p: Path) -> bool:
    """macOS publishes /tmp, /var, and /etc as symlinks into /private as an
    OS convention. Those are OS-owned, not planted, and a scratch root under
    them must not be misreported as an attack."""
    try:
        rel = p.relative_to("/")
    except ValueError:
        return False
    return os.path.realpath(str(p)) == str(Path("/private") / rel)


def _validate_scratch_root(root: Path) -> None:
    """Reject anything a caller-supplied --scratch-dir must not contain: a
    symlink anywhere among its existing path components (the root itself, its
    parent, or any ancestor hop), or a pre-existing non-empty directory.
    Either is how a scratch root can be steered to write through to a target
    the harness does not own — mkdir(parents=True) follows a symlinked
    ancestor silently, so every component that already exists is checked with
    lstat before anything is created. Components that do not exist yet cannot
    be symlinks; mkdir creates them as real directories (and the eval.db
    defense-in-depth check covers the validation-to-use window)."""
    root = root.absolute()
    cur = Path(root.anchor)
    for part in root.relative_to(root.anchor).parts:
        cur = cur / part
        if cur.is_symlink():
            if _ambient_symlink(cur):
                continue
            what = (
                "is itself a symlink"
                if cur == root
                else f"component {cur} is a symlink"
            )
        elif not cur.exists():
            break
        else:
            continue
        raise SystemExit(
            f"refusing --scratch-dir {root}: {what}. This harness never "
            "follows symlinks for scratch storage."
        )
    if root.exists():
        if not root.is_dir():
            raise SystemExit(
                f"refusing --scratch-dir {root}: exists and is not a directory"
            )
        if any(root.iterdir()):
            raise SystemExit(
                f"refusing --scratch-dir {root}: directory is not empty; this "
                "harness only writes into a scratch root it creates fresh."
            )


def _reject_existing_scratch_db(db_path: Path) -> None:
    """Defense in depth alongside _validate_scratch_root: refuse if the
    database file (or a WAL/SHM sidecar) this run is about to create already
    exists — e.g. through a race, or a symlink planted between validation and
    use."""
    for suffix in ("", "-wal", "-shm"):
        candidate = Path(str(db_path) + suffix)
        if candidate.is_symlink() or candidate.exists():
            raise SystemExit(
                f"refusing to run: {candidate} already exists in the scratch "
                "root; this harness only writes into a database file it "
                "creates itself."
            )


def _open_dir_component_nofollow(parent_fd: int, name: str, full_path: Path) -> int:
    """openat-style: open `name` as a directory relative to `parent_fd`,
    refusing to follow a symlink at this single hop (O_NOFOLLOW) — except
    the macOS /tmp,/var,/etc -> /private ambient mapping, which is OS-owned
    rather than attacker-plantable (see `_ambient_symlink`). Returns a new
    directory fd; the caller owns it and must close it.

    O_NOFOLLOW's failure errno for "this is a symlink" is platform-dependent
    (ELOOP on Linux, ENOTDIR on macOS when combined with O_DIRECTORY), so the
    ambient-mapping exception is decided by `lstat`-checking the component
    itself (`_ambient_symlink`, same check `_validate_scratch_root` uses)
    rather than by matching a specific errno.
    """
    try:
        return os.open(
            name, os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC, dir_fd=parent_fd
        )
    except OSError as e:
        if full_path.is_symlink() and _ambient_symlink(full_path):
            return os.open(name, os.O_DIRECTORY | os.O_CLOEXEC, dir_fd=parent_fd)
        raise SystemExit(
            f"refusing --scratch-dir: symlink or non-directory encountered at "
            f"{full_path} while opening the scratch root (TOCTOU guard): {e}"
        ) from e


def open_scratch_root_fd(root: Path) -> int:
    """Open a persistent, race-resistant directory fd for `root`.

    `_validate_scratch_root` checks components with `lstat` and then
    `make_scratch` creates the directory — a concurrent process can swap a
    component for a symlink in the gap between that check and this open.
    Walking every component from the filesystem root via `openat`
    (`dir_fd=`) with `O_NOFOLLOW` closes that gap: each hop is resolved
    relative to the fd already opened for its parent, so a symlink planted
    after validation is refused here instead of silently followed. All
    subsequent scratch file access in this run goes through the returned fd,
    never through `root` as a bare path.
    """
    root = root.absolute()
    fd = os.open(root.anchor, os.O_DIRECTORY | os.O_CLOEXEC)
    cur = Path(root.anchor)
    try:
        for part in root.relative_to(root.anchor).parts:
            cur = cur / part
            next_fd = _open_dir_component_nofollow(fd, part, cur)
            os.close(fd)
            fd = next_fd
        return fd
    except BaseException:
        os.close(fd)
        raise


def _claim_scratch_file(root_fd: int, name: str) -> None:
    """Atomically claim `name` under the scratch root as a fresh, real file.

    `O_CREAT | O_EXCL` refuses if anything — including a symlink — already
    exists at `name`; `O_NOFOLLOW` additionally refuses to follow a symlink
    raced into place on the create call itself. Immediately closed: kkernel
    (subprocess) and sqlite3 need a path string, not an fd (see
    `verified_scratch_path`), so the claim's only job is to make the
    eventual path-based open resolve to a file this run created, not one a
    concurrent process substituted.
    """
    fd = os.open(
        name,
        os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_WRONLY | os.O_CLOEXEC,
        0o600,
        dir_fd=root_fd,
    )
    os.close(fd)


def verified_scratch_path(root: Path, root_fd: int, name: str) -> str:
    """Resolve `name` under the scratch root to an absolute path string,
    re-verified immediately before a subprocess/sqlite3 call that can only
    take a path (neither accepts an fd, and macOS has no /proc/self/fd to
    convert one).

    Opens `name` through the TOCTOU-guarded `root_fd` with `O_NOFOLLOW`
    (refusing a symlink) and `fstat`s that fd; separately `stat`s the
    realpath a subprocess would actually open. If the two do not name the
    same file (device + inode), something was swapped between the fd-based
    claim and this call, and the harness refuses to hand the path to a
    subprocess rather than risk it writing through a symlink.
    """
    try:
        leaf_fd = os.open(
            name, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC, dir_fd=root_fd
        )
    except OSError as e:
        raise SystemExit(
            f"refusing to use {root / name}: could not open {name!r} through the "
            f"validated scratch-root fd without following a symlink (TOCTOU guard): {e}"
        ) from e
    try:
        fd_stat = os.fstat(leaf_fd)
    finally:
        os.close(leaf_fd)
    resolved = os.path.realpath(str(root / name))
    path_stat = os.stat(resolved, follow_symlinks=False)
    if (path_stat.st_dev, path_stat.st_ino) != (fd_stat.st_dev, fd_stat.st_ino):
        raise SystemExit(
            f"refusing to use {resolved}: it no longer matches the file opened "
            "through the validated scratch-root fd (TOCTOU guard) — device/inode "
            "changed between claim and use"
        )
    return resolved


def _rmtree_contents_via_fd(dir_fd: int) -> None:
    """Recursively delete everything inside the directory named by `dir_fd`,
    never resolving a path string and never following a symlink: entries are
    listed with `os.scandir(dir_fd)`, a symlink entry is `unlink`ed as a leaf
    (removing the link itself, not its target), and only entries confirmed
    as real directories via `O_NOFOLLOW` are opened and recursed into."""
    with os.scandir(dir_fd) as it:
        entries = list(it)
    for entry in entries:
        if entry.is_symlink():
            os.unlink(entry.name, dir_fd=dir_fd)
        elif entry.is_dir(follow_symlinks=False):
            child_fd = os.open(
                entry.name, os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC, dir_fd=dir_fd
            )
            try:
                _rmtree_contents_via_fd(child_fd)
            finally:
                os.close(child_fd)
            os.rmdir(entry.name, dir_fd=dir_fd)
        else:
            os.unlink(entry.name, dir_fd=dir_fd)


def write_scratch_jsonl(root_fd: int, name: str, rows: list[dict]) -> None:
    """Create-and-write `name` under the scratch root through `root_fd`.
    `O_CREAT | O_EXCL` refuses anything already at that name (including a
    symlink); `O_NOFOLLOW` additionally refuses a symlink raced into place
    on the create call itself."""
    fd = os.open(
        name,
        os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_WRONLY | os.O_CLOEXEC,
        0o600,
        dir_fd=root_fd,
    )
    with os.fdopen(fd, "w") as f:
        for row in rows:
            f.write(json.dumps(row) + "\n")


def read_scratch_jsonl(root_fd: int, name: str) -> list[dict]:
    """Read `name` under the scratch root through `root_fd` with
    `O_NOFOLLOW`, so this harness's own read never follows a symlink raced
    into place while a subprocess was writing that name."""
    fd = os.open(name, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC, dir_fd=root_fd)
    with os.fdopen(fd) as f:
        return [json.loads(line) for line in f if line.strip()]


def cleanup_scratch(root: Path, root_fd: int) -> None:
    """Delete the scratch tree through the held, TOCTOU-guarded `root_fd`
    rather than a fresh path-based walk, then remove the now-empty root
    directory itself. The final `rmdir` uses `root`'s path rather than a
    parent fd (this script never opens the scratch root's parent), but
    `rmdir` only ever removes an empty directory — a symlink or a
    repopulated directory raced into that name fails closed instead of
    deleting through it."""
    _rmtree_contents_via_fd(root_fd)
    os.rmdir(root)


def make_scratch(scratch_dir: str | None) -> Path:
    if scratch_dir:
        root = Path(scratch_dir)
        _validate_scratch_root(root)
        root.mkdir(parents=True, exist_ok=True)
    else:
        root = Path(tempfile.mkdtemp(prefix=SCRATCH_MARKER))
    (root / "home").mkdir(exist_ok=True)
    return root


def scratch_env(root: Path, db_path: Path) -> dict:
    """Minimal allowlisted child environment: PATH (so the `kkernel` binary
    name resolves), HOME/TMPDIR redirected into the scratch root, and the
    harness's own explicit KHIVE_DB/KHIVE_EMBEDDING_MODEL. Nothing else from
    the caller's environment is copied through, so no inherited KHIVE_* or
    other retrieval-affecting variable can change what gets scored."""
    tmp_dir = root / "tmp"
    tmp_dir.mkdir(exist_ok=True)
    return {
        "PATH": os.environ.get("PATH", ""),
        "HOME": str(root / "home"),
        "TMPDIR": str(tmp_dir),
        "KHIVE_DB": str(db_path),
        "KHIVE_EMBEDDING_MODEL": PINNED_EMBEDDING_MODEL,
    }


def run_kkernel(
    kkernel: str, args: list[str], env: dict, **kw
) -> subprocess.CompletedProcess:
    cmd = [kkernel, *args, "--log", "error"]
    result = subprocess.run(
        cmd, env=env, capture_output=True, text=True, check=False, **kw
    )
    if result.returncode != 0:
        raise RuntimeError(
            f"command failed ({result.returncode}): {' '.join(cmd)}\n"
            f"stdout: {result.stdout}\nstderr: {result.stderr}"
        )
    return result


def seed_corpus(
    kkernel: str,
    env: dict,
    root: Path,
    root_fd: int,
    seed: int,
    epoch: str,
) -> list[dict]:
    notes = generate_corpus.build_corpus(DEFAULT_QUERIES, seed, epoch)
    if len(notes) != 400:
        raise SystemExit(f"expected 400 notes, generator produced {len(notes)}")
    ops = [
        {
            "tool": "memory.remember",
            "args": {
                "content": n["content"],
                "salience": n["salience"],
                "decay_factor": n["decay_factor"],
                "memory_type": n["memory_type"],
                "tags": [f"k:{n['key']}"],
            },
        }
        for n in notes
    ]
    write_scratch_jsonl(root_fd, "seed_ops.jsonl", ops)
    _claim_scratch_file(root_fd, "seed_save.jsonl")

    run_kkernel(
        kkernel,
        [
            "exec",
            "--ops-file",
            verified_scratch_path(root, root_fd, "seed_ops.jsonl"),
            "--save-file",
            verified_scratch_path(root, root_fd, "seed_save.jsonl"),
            "--db",
            verified_scratch_path(root, root_fd, "eval.db"),
        ],
        env,
    )

    rows = read_scratch_jsonl(root_fd, "seed_save.jsonl")
    failed = [r for r in rows if not r.get("ok")]
    if failed:
        raise RuntimeError(f"{len(failed)} seed ops failed, e.g. {failed[0]}")
    if len(rows) != 400:
        raise RuntimeError(f"expected 400 seed rows, got {len(rows)}")

    return notes


def key_id_map(root: Path, root_fd: int) -> dict[str, str]:
    conn = sqlite3.connect(verified_scratch_path(root, root_fd, "eval.db"))
    try:
        cur = conn.execute(
            "SELECT id, properties FROM notes WHERE properties IS NOT NULL"
        )
        mapping: dict[str, str] = {}
        for note_id, props_json in cur.fetchall():
            props = json.loads(props_json)
            for tag in props.get("tags", []):
                if tag.startswith("k:"):
                    mapping[tag[2:]] = note_id
        return mapping
    finally:
        conn.close()


def set_ages(
    root: Path, root_fd: int, notes: list[dict], key_to_id: dict[str, str]
) -> None:
    conn = sqlite3.connect(verified_scratch_path(root, root_fd, "eval.db"))
    try:
        for n in notes:
            note_id = key_to_id[n["key"]]
            dt = datetime.fromisoformat(n["created_at_iso"].replace("Z", "+00:00"))
            micros = int(dt.timestamp() * 1_000_000)
            conn.execute(
                "UPDATE notes SET created_at = ?, updated_at = ? WHERE id = ?",
                (micros, micros, note_id),
            )
        conn.commit()
    finally:
        conn.close()


def run_condition(
    kkernel: str,
    env: dict,
    root: Path,
    root_fd: int,
    condition: str,
    queries: list[dict],
    corpus_size: int,
) -> list[dict]:
    extra_args = CONDITIONS[condition]
    ops_name = f"query_ops_{condition}.jsonl"
    save_name = f"query_save_{condition}.jsonl"
    ops = [
        {
            "tool": "memory.recall",
            "args": {
                "query": q["query"],
                "top_k": CANDIDATE_POOL,
                "include_breakdown": True,
                "config": HERMETIC_RECALL_CONFIG,
                **extra_args,
            },
        }
        for q in queries
    ]
    write_scratch_jsonl(root_fd, ops_name, ops)
    _claim_scratch_file(root_fd, save_name)

    run_kkernel(
        kkernel,
        [
            "exec",
            "--ops-file",
            verified_scratch_path(root, root_fd, ops_name),
            "--save-file",
            verified_scratch_path(root, root_fd, save_name),
            "--db",
            verified_scratch_path(root, root_fd, "eval.db"),
        ],
        env,
    )

    rows = read_scratch_jsonl(root_fd, save_name)
    if len(rows) != len(queries):
        raise RuntimeError(f"expected {len(queries)} query rows, got {len(rows)}")
    for q, row in zip(queries, rows):
        row["_hits"] = _validate_recall_row(q, row, corpus_size)
    return rows


def _validate_recall_row(query: dict, row: dict, corpus_size: int) -> list:
    """Validate a memory.recall response row and return its hit list.

    The normal (non-degraded, non-budget-capped) response is a bare JSON
    array of hits — `to_json(&results)`, the final return in
    crates/khive-pack-memory/src/handlers/recall.rs `handle_recall`. A small
    set of edge cases (ANN degraded, budget-capped to zero hits, or verbose
    diagnostics) instead return an object with a top-level "results" key
    plus flags, specifically so a degraded/capped response isn't
    indistinguishable from a genuine bare-array no-match.

    This harness runs a fixed top_k against its own fresh 400-note corpus,
    so none of those edge cases are expected; hitting one here means the
    run is not representative (e.g. a cold/unready ANN index) and must fail
    loudly rather than silently score a degraded ranking into gold.

    A bare array is accepted at any length by the response shape itself —
    the response token budget (crates/khive-pack-memory/src/handlers/
    recall.rs `handle_recall`, the `budget_cutoff` loop after ranking) can
    still truncate a non-empty hit list below the requested top_k, stamping
    each surviving hit with `"truncated": true` rather than changing the
    envelope shape. Silently scoring that shorter pool would mislabel
    Recall@100 / TargetRecall@100 as if the full pool had been evaluated, so
    both the pool depth and the per-hit marker are checked explicitly below.
    """
    if not row.get("ok"):
        raise RuntimeError(
            f"memory.recall failed for query_id={query['query_id']!r}: "
            f"{row.get('error')!r}"
        )
    result = row.get("result")
    if isinstance(result, list):
        hits = result
    elif isinstance(result, dict):
        if result.get("degraded"):
            raise RuntimeError(
                f"memory.recall degraded for query_id={query['query_id']!r}: "
                f"{result.get('degraded_reason')!r} — this harness requires a "
                "fully warm, non-degraded index; re-run once indexing settles"
            )
        if result.get("truncated"):
            raise RuntimeError(
                f"memory.recall budget-capped to zero hits for "
                f"query_id={query['query_id']!r}; unexpected against this "
                "harness's fixed top_k on its fixed corpus"
            )
        hits = result.get("results")
        if not isinstance(hits, list):
            raise RuntimeError(
                f"memory.recall returned an unexpected response shape for "
                f"query_id={query['query_id']!r} (got {type(result).__name__}); "
                "expected a bare hit array or a known degraded/truncated envelope"
            )
    else:
        raise RuntimeError(
            f"memory.recall returned an unexpected response shape for "
            f"query_id={query['query_id']!r} (got {type(result).__name__}); "
            "expected a bare hit array or a known degraded/truncated envelope"
        )

    expected_pool = min(CANDIDATE_POOL, corpus_size)
    if len(hits) < expected_pool:
        raise RuntimeError(
            f"memory.recall pool for query_id={query['query_id']!r} is truncated: "
            f"got {len(hits)} hits, expected {expected_pool} (top_k={CANDIDATE_POOL} "
            f"against a {corpus_size}-note corpus) — scoring this pool would silently "
            "mislabel Recall@100/TargetRecall@100; raise the recall token budget "
            "(HERMETIC_RECALL_CONFIG) or investigate the cause before re-running"
        )
    if any(h.get("truncated") for h in hits):
        raise RuntimeError(
            f"memory.recall pool for query_id={query['query_id']!r} carries per-hit "
            f"'truncated' markers at depth {len(hits)}; the harness requires a fully "
            "untruncated top-100 pool for a valid Recall@100 label"
        )
    return hits


def dcg_at_k(grades: list[int], k: int) -> float:
    total = 0.0
    for i, g in enumerate(grades[:k], start=1):
        gain = (2**g) - 1
        total += gain / math.log2(i + 1)
    return total


def compute_metrics(query: dict, id_to_key: dict[str, str], result_row: dict) -> dict:
    labels = query["labels"]
    hits = result_row["_hits"]
    hit_ids = [h["id"] for h in hits]
    hit_grades = [labels.get(id_to_key.get(hid, ""), 0) for hid in hit_ids]

    ideal_grades = sorted(labels.values(), reverse=True)
    idcg10 = dcg_at_k(ideal_grades, 10)
    dcg10 = dcg_at_k(hit_grades, 10)
    ndcg10 = (dcg10 / idcg10) if idcg10 > 0 else 0.0

    total_relevant = sum(1 for g in labels.values() if g >= 1)
    retrieved_relevant = sum(1 for g in hit_grades if g >= 1)
    recall100 = (retrieved_relevant / total_relevant) if total_relevant > 0 else 0.0

    total_targets = sum(1 for g in labels.values() if g >= 2)
    retrieved_targets = sum(1 for g in hit_grades if g >= 2)
    target_recall100 = (retrieved_targets / total_targets) if total_targets > 0 else 0.0

    mrr10 = 0.0
    for i, g in enumerate(hit_grades[:10], start=1):
        if g >= 2:
            mrr10 = 1.0 / i
            break

    return {
        "query_id": query["query_id"],
        "cluster": query["cluster"],
        "query_class": query["query_class"],
        "candidate_count": len(hits),
        "nDCG@10": ndcg10,
        "Recall@100": recall100,
        "TargetRecall@100": target_recall100,
        "MRR@10": mrr10,
    }


def aggregate(rows: list[dict]) -> dict:
    metrics = ["nDCG@10", "Recall@100", "TargetRecall@100", "MRR@10"]

    def mean(vals: list[float]) -> float:
        return sum(vals) / len(vals) if vals else 0.0

    overall = {m: mean([r[m] for r in rows]) for m in metrics}
    by_class: dict[str, dict] = {}
    grouped = defaultdict(list)
    for r in rows:
        grouped[r["query_class"]].append(r)
    for cls, cls_rows in sorted(grouped.items()):
        by_class[cls] = {m: mean([r[m] for r in cls_rows]) for m in metrics}
    return {"overall": overall, "by_class": by_class}


def print_table(condition: str, agg: dict) -> None:
    metrics = ["nDCG@10", "Recall@100", "TargetRecall@100", "MRR@10"]
    print(f"\n== {condition} — overall ==")
    print("| Metric | Value |")
    print("| --- | --- |")
    for m in metrics:
        print(f"| {m} | {agg['overall'][m]:.4f} |")
    print(f"\n== {condition} — per query_class ==")
    print("| query_class | " + " | ".join(metrics) + " |")
    print("| --- | " + " | ".join("---" for _ in metrics) + " |")
    for cls, vals in agg["by_class"].items():
        print(f"| {cls} | " + " | ".join(f"{vals[m]:.4f}" for m in metrics) + " |")


_REVISION_RE = re.compile(r"revision ([0-9a-f]+)")


def get_kkernel_version(kkernel: str) -> str:
    result = subprocess.run(
        [kkernel, "--version"], capture_output=True, text=True, check=False
    )
    if result.returncode != 0:
        raise RuntimeError(f"failed to read `{kkernel} --version`: {result.stderr}")
    return result.stdout.strip()


def kkernel_revision(version_str: str) -> str:
    """Extract the git revision hash from `kkernel --version` output, e.g.
    "kkernel 0.7.0 (revision d25b837.., built 2026-08-15T02:10:51Z)" ->
    "d25b837..". Comparing on just the revision (not the full string) means
    gold survives a same-commit rebuild, which always gets a fresh build
    timestamp; the revision hash is what actually identifies drift."""
    m = _REVISION_RE.search(version_str)
    return m.group(1) if m else version_str


def version_drift_note(gold: dict, kkernel_version: str) -> str | None:
    """Context, not a verdict: the gate's question is whether the metrics are
    preserved, so a revision that differs from gold's recorded one must not
    fail the check by itself — every commit after the gold-derivation commit
    (including the commit that ships the gold file) produces a different
    revision hash while leaving retrieval behavior untouched. The note exists
    to keep a metric mismatch from being misdiagnosed when the binary really
    did drift."""
    gold_version = gold.get("kkernel_version")
    if gold_version is None:
        return (
            "kkernel_version: gold file has no kkernel_version field, so binary "
            "identity was not checked for this comparison — a legacy or "
            "hand-edited gold file carries no provenance; re-derive it with "
            "--write-gold to record the binary it comes from"
        )
    if kkernel_revision(gold_version) != kkernel_revision(kkernel_version):
        return (
            f"kkernel_version: got {kkernel_version!r} vs gold {gold_version!r} — "
            "the kkernel binary this run used does not match the one gold was "
            "derived against (build/version drift, not necessarily a metric "
            "regression; verify the binary before treating any metric "
            "mismatches as real)"
        )
    return None


def compare_gold(agg: dict, gold: dict, tol: float) -> list[str]:
    mismatches = []
    for m, v in agg["overall"].items():
        gv = gold["overall"][m]
        if abs(v - gv) > tol:
            mismatches.append(f"overall.{m}: got {v!r} vs gold {gv!r}")
    for cls, vals in agg["by_class"].items():
        for m, v in vals.items():
            gv = gold["by_class"][cls][m]
            if abs(v - gv) > tol:
                mismatches.append(f"{cls}.{m}: got {v!r} vs gold {gv!r}")
    return mismatches


def _expect_systemexit(fn, *a, **kw) -> tuple[bool, str]:
    try:
        fn(*a, **kw)
    except SystemExit as e:
        return True, str(e)
    return False, "did not raise SystemExit"


def _record(failures: list[str], name: str, ok: bool, msg: str) -> None:
    if not ok:
        failures.append(f"{name}: {msg}")


def run_self_tests() -> int:
    """Scratch-dir / KHIVE_DB safety regression checks. Pure Python, no
    kkernel binary required — exercises the validation functions directly."""
    failures: list[str] = []

    # 1. an inherited KHIVE_DB pointing outside the scratch dir is refused.
    with tempfile.TemporaryDirectory() as td:
        victim = Path(td) / "external.db"
        victim.touch()
        old = os.environ.get("KHIVE_DB")
        os.environ["KHIVE_DB"] = str(victim)
        try:
            ok, msg = _expect_systemexit(refuse_unsafe_db_env)
        finally:
            if old is None:
                os.environ.pop("KHIVE_DB", None)
            else:
                os.environ["KHIVE_DB"] = old
        _record(failures, "inherited-khive-db-outside-scratch-refused", ok, msg)

    # 2. a pre-existing --scratch-dir root containing an eval.db symlink to an
    # external file is refused, and the external file is left untouched.
    with tempfile.TemporaryDirectory() as td:
        root = Path(td) / "root"
        root.mkdir()
        victim = Path(td) / "victim.db"
        victim.touch()
        (root / "eval.db").symlink_to(victim)
        ok, msg = _expect_systemexit(make_scratch, str(root))
        _record(failures, "preexisting-eval-db-symlink-refused", ok, msg)
        if victim.stat().st_size != 0:
            failures.append(
                "preexisting-eval-db-symlink-refused: victim file was written to"
            )

    # 3. a --scratch-dir root that is itself a symlink is refused.
    with tempfile.TemporaryDirectory() as td:
        real = Path(td) / "real"
        real.mkdir()
        link = Path(td) / "link"
        link.symlink_to(real)
        ok, msg = _expect_systemexit(make_scratch, str(link))
        _record(failures, "symlinked-scratch-root-refused", ok, msg)

    # 4. a pre-existing non-empty --scratch-dir root (no eval.db involved) is
    # refused too.
    with tempfile.TemporaryDirectory() as td:
        root = Path(td) / "root"
        root.mkdir()
        (root / "junk").write_text("x")
        ok, msg = _expect_systemexit(make_scratch, str(root))
        _record(failures, "nonempty-scratch-root-refused", ok, msg)

    # 5. a fresh / nonexistent --scratch-dir root is accepted and initialized.
    with tempfile.TemporaryDirectory() as td:
        root = Path(td) / "fresh"
        try:
            made = make_scratch(str(root))
            ok = made.exists() and (made / "home").is_dir()
            msg = "ok" if ok else "scratch dir not initialized correctly"
        except SystemExit as e:
            ok, msg = False, f"unexpected refusal: {e}"
        _record(failures, "fresh-scratch-root-accepted", ok, msg)

    # 6. a pre-existing eval.db (regular file, not a symlink) in an otherwise
    # freshly-created scratch root is refused by the defense-in-depth check.
    with tempfile.TemporaryDirectory() as td:
        root = Path(td) / "root"
        root.mkdir()
        (root / "eval.db").write_text("not a real db")
        db_path = root / "eval.db"
        ok, msg = _expect_systemexit(_reject_existing_scratch_db, db_path)
        _record(failures, "preexisting-eval-db-file-refused", ok, msg)

    # 7. a nonexistent leaf below a symlinked PARENT is refused, and nothing
    # is created behind the symlink — mkdir(parents=True) would otherwise
    # follow the parent hop silently.
    with tempfile.TemporaryDirectory() as td:
        victim = Path(td) / "victim"
        victim.mkdir()
        link = Path(td) / "link"
        link.symlink_to(victim)
        ok, msg = _expect_systemexit(make_scratch, str(link / "new"))
        _record(failures, "symlinked-parent-of-new-leaf-refused", ok, msg)
        if any(victim.iterdir()):
            failures.append(
                "symlinked-parent-of-new-leaf-refused: something was created "
                "behind the symlinked parent"
            )

    # 8. the macOS ambient /tmp -> /private/tmp mapping is tolerated: a fresh
    # root under /tmp validates (on Linux /tmp is a real directory and passes
    # trivially, so the check is meaningful on both).
    with tempfile.TemporaryDirectory(dir="/tmp") as td:
        candidate = Path(td) / "fresh"
        try:
            _validate_scratch_root(candidate)
            ok, msg = True, "ok"
        except SystemExit as e:
            ok, msg = False, f"ambient /tmp ancestry misreported as planted: {e}"
        _record(failures, "ambient-tmp-ancestry-accepted", ok, msg)

    # 9. a gold file with no kkernel_version yields a drift note (binary
    # identity unchecked must be said, never silently treated as verified).
    note = version_drift_note({}, "kkernel 0.0.0 (revision 0000000, built x)")
    _record(
        failures,
        "gold-missing-kkernel-version-noted",
        note is not None and "no kkernel_version" in note,
        f"expected a missing-provenance note, got {note!r}",
    )

    # 10. a file swapped for a symlink to an external victim AFTER it was
    # claimed through the validated root fd is refused at use time
    # (`verified_scratch_path`), and the victim is left untouched — this is
    # the TOCTOU window between validation and use, closed by O_NOFOLLOW on
    # the fd-relative leaf open.
    with tempfile.TemporaryDirectory() as td:
        root = Path(td) / "root"
        root.mkdir()
        root_fd = open_scratch_root_fd(root)
        try:
            _claim_scratch_file(root_fd, "claimed.txt")
            victim = Path(td) / "victim.txt"
            victim.write_text("do not touch")
            claimed_path = root / "claimed.txt"
            claimed_path.unlink()
            claimed_path.symlink_to(victim)
            ok, msg = _expect_systemexit(
                verified_scratch_path, root, root_fd, "claimed.txt"
            )
            _record(
                failures, "post-claim-symlink-swap-refused-at-use", ok, msg
            )
            if victim.read_text() != "do not touch":
                failures.append(
                    "post-claim-symlink-swap-refused-at-use: victim file was modified"
                )
        finally:
            os.close(root_fd)

    # 11. the device+inode re-check arm: the scratch root directory itself is
    # swapped out from under a held `root_fd` (renamed aside, then recreated
    # fresh at the same path) between the fd-based claim and use — a regular
    # file, not a symlink, so O_NOFOLLOW alone would not catch it. The fd-
    # resolved and path-resolved views of the same name now disagree on
    # device+inode, and `verified_scratch_path` must refuse rather than hand
    # the swapped path to a subprocess.
    with tempfile.TemporaryDirectory() as td:
        root = Path(td) / "root"
        root.mkdir()
        root_fd = open_scratch_root_fd(root)
        try:
            _claim_scratch_file(root_fd, "claimed.txt")
            moved_aside = Path(td) / "root-moved-aside"
            root.rename(moved_aside)
            root.mkdir()
            (root / "claimed.txt").write_text("swapped root")
            ok, msg = _expect_systemexit(
                verified_scratch_path, root, root_fd, "claimed.txt"
            )
            _record(failures, "device-inode-mismatch-refused", ok, msg)
        finally:
            os.close(root_fd)

    if failures:
        print(f"\nSELF-TEST FAILED ({len(failures)}):", file=sys.stderr)
        for f in failures:
            print(f"  {f}", file=sys.stderr)
        return 1
    print("\nSELF-TEST PASSED (11 checks)")
    return 0


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--kkernel", default="kkernel")
    ap.add_argument("--queries", type=Path, default=DEFAULT_QUERIES)
    ap.add_argument("--condition", default="A_fused_direct", choices=sorted(CONDITIONS))
    ap.add_argument(
        "--out", type=Path, default=None, help="per-query result JSONL output path"
    )
    ap.add_argument("--seed", type=int, default=generate_corpus.DEFAULT_SEED)
    ap.add_argument("--epoch", type=str, default=generate_corpus.DEFAULT_EPOCH)
    ap.add_argument("--scratch-dir", default=None)
    ap.add_argument("--keep-scratch", action="store_true")
    ap.add_argument("--check-gold", action="store_true")
    ap.add_argument("--gold", type=Path, default=DEFAULT_GOLD)
    ap.add_argument("--gold-tolerance", type=float, default=0.0)
    ap.add_argument(
        "--write-gold",
        type=Path,
        default=None,
        help="write the aggregate result (incl. kkernel_version) as gold-shaped JSON",
    )
    ap.add_argument(
        "--self-test",
        action="store_true",
        help="run scratch-dir safety regression checks and exit (no kkernel needed)",
    )
    args = ap.parse_args()

    if args.self_test:
        return run_self_tests()

    kkernel_version = get_kkernel_version(args.kkernel)
    print(f"kkernel version: {kkernel_version}")

    refuse_unsafe_db_env()
    root = make_scratch(args.scratch_dir)
    # Opened immediately after creation so every subsequent scratch access in
    # this run goes through a TOCTOU-guarded fd rather than re-resolving
    # `root` as a bare path (see `open_scratch_root_fd`).
    root_fd = open_scratch_root_fd(root)
    try:
        db_path = root / "eval.db"
        _reject_existing_scratch_db(db_path)
        _claim_scratch_file(root_fd, "eval.db")
        env = scratch_env(root, db_path)

        try:
            run_kkernel(
                args.kkernel,
                ["db", "migrate", "--db", verified_scratch_path(root, root_fd, "eval.db")],
                env,
            )
            queries = generate_corpus.parse_queries(args.queries)
            notes = seed_corpus(args.kkernel, env, root, root_fd, args.seed, args.epoch)
            key_to_id = key_id_map(root, root_fd)
            missing = [n["key"] for n in notes if n["key"] not in key_to_id]
            if missing:
                raise RuntimeError(
                    f"{len(missing)} seeded notes missing tag-based id, e.g. {missing[:5]}"
                )
            set_ages(root, root_fd, notes, key_to_id)
            id_to_key = {v: k for k, v in key_to_id.items()}

            result_rows = run_condition(
                args.kkernel, env, root, root_fd, args.condition, queries, len(notes)
            )
            per_query = [
                compute_metrics(q, id_to_key, row)
                for q, row in zip(queries, result_rows)
            ]
        finally:
            if not args.keep_scratch:
                cleanup_scratch(root, root_fd)
    finally:
        os.close(root_fd)

    agg = aggregate(per_query)
    print_table(args.condition, agg)

    if args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        with args.out.open("w") as f:
            for r in per_query:
                f.write(json.dumps({"condition": args.condition, **r}) + "\n")
        print(f"\nwrote {len(per_query)} rows to {args.out}")

    if args.write_gold:
        args.write_gold.parent.mkdir(parents=True, exist_ok=True)
        gold_out = {"kkernel_version": kkernel_version, **agg}
        args.write_gold.write_text(
            json.dumps(gold_out, indent=2, sort_keys=True) + "\n"
        )
        print(f"\nwrote gold to {args.write_gold}")

    if args.check_gold:
        if not args.gold.exists():
            print(f"\ngold file not found: {args.gold}", file=sys.stderr)
            return 2
        gold = json.loads(args.gold.read_text())
        drift = version_drift_note(gold, kkernel_version)
        mismatches = compare_gold(agg, gold, args.gold_tolerance)
        if mismatches:
            print(
                f"\nGOLD CHECK FAILED ({len(mismatches)} mismatches):", file=sys.stderr
            )
            if drift:
                print(f"  context: {drift}", file=sys.stderr)
            for m in mismatches:
                print(f"  {m}", file=sys.stderr)
            return 1
        if drift:
            print(f"\nWARNING: {drift}", file=sys.stderr)
        print(
            "\nGOLD CHECK PASSED — matches gold/A_fused_direct.json within tolerance "
            f"{args.gold_tolerance}"
        )

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
