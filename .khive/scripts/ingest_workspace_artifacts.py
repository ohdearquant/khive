#!/usr/bin/env python3
"""Canonical ingest of .khive/ workspace artifacts into khive.db (workspace pack).

Migrates loose, gitignored `.khive/` files (codex verdicts, workspace docs,
session summaries/handoffs, crate audits) into queryable khive.db records:
one `workspace` entity per lane (date/topic grouping), one note per file,
`annotates` edges from each note back to its lane entity, and (for codex
verdicts that name a PR) an additional `annotates` edge to the matching
git-pack `pull_request` note.

No new MCP verbs are needed — this uses only the existing generic
create/link/list/search verbs over the merged workspace-pack substrate
(Ocean ruling 2026-07-13).

Usage:
    uv run python .khive/scripts/ingest_workspace_artifacts.py --dry-run
    uv run python .khive/scripts/ingest_workspace_artifacts.py --live

Design notes:
- DRY-RUN IS THE DEFAULT. --live is required to perform any khive writes.
- All writes go through `kkernel exec '<ops-DSL>'` (subprocess). This script
  never opens khive.db directly; sqlite3 is used nowhere.
- Idempotency key = (source_path, content_sha256_16). A resume cursor file
  records completed (source_path, sha16) pairs so a re-run after interruption
  skips already-ingested files without any server round-trip. As a second,
  server-side safety net (covers a lost/stale cursor file), --live also
  bulk-loads existing ws-ingest-tagged notes once at startup, keyed to their
  note id. A file whose key is already present there is NOT accepted as
  complete on sight: its note's outgoing `annotates` edges are checked
  (`neighbors`, direction outgoing, relation annotates) against the targets
  this artifact requires (workspace lane, and PR for codex verdicts), and
  whatever is missing is created before the cursor is backfilled. This
  covers a note left incomplete by a pre-fix run, or by a create-time link
  failure whose runtime-side compensation was itself best-effort and did not
  run to completion (crates/khive-runtime/src/operations.rs).
- Content is truncated at 32,768 bytes (of the FINAL UTF-8-encoded,
  replacement-decoded payload, marker space reserved — the daemon embedder's
  actual limit, which is byte-counted despite its "chars" error wording) with
  a trailing marker; the sha256 is always computed over the ORIGINAL
  (untruncated) bytes so the dedup key is stable.
- Note + its `annotates` edges (workspace lane, and PR for codex verdicts)
  are created in ONE `create(..., annotates=[...])` call. The runtime
  validates every annotate target before any write and compensates (deletes
  the note row) if an edge fails, so a note can never be persisted with a
  missing required edge (crates/khive-runtime/src/operations.rs
  create_note_inner).
- CONCURRENCY: a machine-wide POSIX advisory lock (fcntl.flock on the fixed
  path `/tmp/lion-khive-wsingest.lock`, independent of which checkout/
  worktree the script runs from) excludes concurrent --live invocations on
  this host. This is NOT a substitute for a server-side atomic natural-key
  dedup: the generic `create` verb does not expose the `external_id` /
  `try_insert_note` atomic-insert path (that path exists only on the
  internal `try_create_note` runtime method, unused by the MCP `create`
  verb). Two --live runs on DIFFERENT hosts against the same khive.db are
  NOT excluded and can still race to create duplicate notes for the same
  (source_path, sha16). Product gap, not fixed here — see PR body.
- PR annotation is scoped to THIS repository's `project` entity (exact-name
  match against WS_INGEST_PROJECT_NAME, default "khive-oss"). A codex-verdict
  file naming PR #N only annotates a `pull_request` note whose
  properties.project_id matches that project; a same-numbered PR in another
  repository's project is never used (mirrors khive-pack-git ingest.rs
  find_by_number's project_id scoping).
"""

from __future__ import annotations

import argparse
import fcntl
import hashlib
import json
import os
import re
import subprocess
import sys
from dataclasses import dataclass, field
from datetime import UTC, datetime
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
REPO_ROOT = SCRIPT_DIR.parent.parent  # .khive/scripts -> .khive -> repo root
KHIVE_DIR = REPO_ROOT / ".khive"
CURSOR_PATH = SCRIPT_DIR / ".ws_ingest_cursor"
# Fixed, checkout-independent path: multiple worktrees of this repo (or
# multiple repos, incidentally) share the same khive.db, so the exclusion
# lock must be machine-wide, not per-checkout. /tmp, lion-prefixed, never
# inside a cargo target tree (fleet lock-naming convention).
LOCK_PATH = Path("/tmp/lion-khive-wsingest.lock")
KKERNEL = os.path.expanduser("~/.cargo/bin/kkernel")

# The daemon's embedder rejects create() content whose UTF-8 BYTE length
# exceeds 32,768: lattice-embed 0.6.1 (service/cached.rs, native.rs) checks
# `text.len()` — Rust String BYTES — even though its error message says
# "chars". Observed live 2026-07-16: a 36,556-byte ASCII doc was rejected,
# and a char-capped 32,768-char doc still failed at 32,846 (bytes, from
# multibyte expansion). The cap therefore applies to the final encoded
# byte length, at the embedder's actual limit.
MAX_CONTENT_BYTES = 32_768
TRUNCATION_MARKER = "\n\n...[truncated by ws-ingest, original length {orig} bytes]...\n"

INGEST_TAG = "ws-ingest"

# This checkout's canonical `project` entity name (exact match). PR/issue
# notes carry `properties.project_id`; PR numbers are only unique within a
# project, so annotation must be scoped to this one, never number-only.
WS_INGEST_PROJECT_NAME = os.environ.get("WS_INGEST_PROJECT_NAME", "khive-oss")

# artifact_class -> canonical note kind (closed set: observation/insight/
# question/decision/reference). Verdicts, workspace docs, and audits are all
# "reference" (ingested-document semantics); summaries/handoffs are
# "observation" (session-context semantics).
NOTE_KIND = {
    "codex_verdict": "reference",
    "workspace_doc": "reference",
    "summary": "observation",
    "handoff": "observation",
    "audit": "reference",
}

PR_NUM_RE = re.compile(r"codex_review_pr(\d+)")


@dataclass
class Artifact:
    artifact_class: str
    path: Path  # absolute
    rel_path: str  # posix, relative to REPO_ROOT
    lane: str
    pr_number: int | None = None
    sha16: str = ""
    size: int = 0


def sha256_16(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()[:16]


def dsl_escape(s: str) -> str:
    """Escape a string for embedding as a double-quoted DSL literal.

    Backslash/quote/newline/CR are JSON-escaped (CR as \\r, never dropped —
    dropping it would silently alter CRLF content while the sha256 dedup key
    still reflects the original bytes). A value that is, in full, a `$prev`
    chain reference (`$prev`, `$prev.<path>`, `$prev[<idx>]...`) would
    otherwise be promoted by the DSL parser into a chain reference instead of
    the literal string (khive-request parser_impl.rs string_as_prev_ref); the
    parser's own documented escape is a single leading backslash
    (parser_impl.rs: `s.strip_prefix('\\')` then re-checks the $prev
    prefixes), so such values get one extra literal backslash prepended.
    """
    escaped = (
        s.replace("\\", "\\\\").replace('"', '\\"').replace("\n", "\\n").replace("\r", "\\r")
    )
    if s == "$prev" or s.startswith("$prev.") or s.startswith("$prev["):
        escaped = "\\\\" + escaped
    return escaped


def dsl_str(s: str) -> str:
    return f'"{dsl_escape(s)}"'


def lane_for(artifact_class: str, rel: Path) -> str:
    parts = rel.parts
    if artifact_class == "workspace_doc":
        idx = parts.index("workspaces")
        if len(parts) > idx + 2:
            return f"{parts[idx + 1]}/{parts[idx + 2]}"
        if len(parts) > idx + 1:
            return parts[idx + 1]
        return "workspaces/misc"
    if artifact_class == "audit":
        idx = parts.index("audits")
        if len(parts) > idx + 2:
            return f"audits/{parts[idx + 1]}/{parts[idx + 2]}"
        if len(parts) > idx + 1:
            return f"audits/{parts[idx + 1]}"
        return "audits/misc"
    if artifact_class == "codex_verdict":
        return "codex-reviews"
    if artifact_class == "summary":
        return "notes/summaries"
    if artifact_class == "handoff":
        return "notes/handoffs"
    raise ValueError(f"unknown artifact_class {artifact_class!r}")


def discover() -> list[Artifact]:
    found: list[Artifact] = []

    codex_dir = KHIVE_DIR / "codex_reviews"
    if codex_dir.is_dir():
        for p in sorted(codex_dir.glob("codex_review_pr*.md")):
            rel = p.relative_to(REPO_ROOT)
            m = PR_NUM_RE.search(p.name)
            pr_number = int(m.group(1)) if m else None
            found.append(
                Artifact(
                    artifact_class="codex_verdict",
                    path=p,
                    rel_path=rel.as_posix(),
                    lane=lane_for("codex_verdict", rel),
                    pr_number=pr_number,
                )
            )

    ws_root = KHIVE_DIR / "workspaces"
    if ws_root.is_dir():
        for p in sorted(ws_root.rglob("*.md")):
            if not p.is_file():
                continue
            rel = p.relative_to(REPO_ROOT)
            found.append(
                Artifact(
                    artifact_class="workspace_doc",
                    path=p,
                    rel_path=rel.as_posix(),
                    lane=lane_for("workspace_doc", rel),
                )
            )

    summaries_dir = KHIVE_DIR / "notes" / "summaries"
    if summaries_dir.is_dir():
        for p in sorted(summaries_dir.glob("*.md")):
            if p.name == "_TEMPLATE.md":
                continue
            rel = p.relative_to(REPO_ROOT)
            found.append(
                Artifact(
                    artifact_class="summary",
                    path=p,
                    rel_path=rel.as_posix(),
                    lane=lane_for("summary", rel),
                )
            )

    handoffs_dir = KHIVE_DIR / "notes" / "handoffs"
    if handoffs_dir.is_dir():
        for p in sorted(handoffs_dir.glob("*.md")):
            rel = p.relative_to(REPO_ROOT)
            found.append(
                Artifact(
                    artifact_class="handoff",
                    path=p,
                    rel_path=rel.as_posix(),
                    lane=lane_for("handoff", rel),
                )
            )

    audits_dir = KHIVE_DIR / "audits"
    if audits_dir.is_dir():
        for p in sorted(audits_dir.rglob("*.md")):
            if not p.is_file():
                continue
            rel = p.relative_to(REPO_ROOT)
            found.append(
                Artifact(
                    artifact_class="audit",
                    path=p,
                    rel_path=rel.as_posix(),
                    lane=lane_for("audit", rel),
                )
            )

    for a in found:
        data = a.path.read_bytes()
        a.size = len(data)
        a.sha16 = sha256_16(data)

    return found


def load_cursor() -> set[tuple[str, str]]:
    done: set[tuple[str, str]] = set()
    if CURSOR_PATH.exists():
        for line in CURSOR_PATH.read_text().splitlines():
            line = line.strip()
            if not line or "\t" not in line:
                continue
            path, sha = line.split("\t", 1)
            done.add((path, sha))
    return done


def append_cursor(rel_path: str, sha16: str) -> None:
    with CURSOR_PATH.open("a") as fh:
        fh.write(f"{rel_path}\t{sha16}\n")


class KKernel:
    """Thin wrapper around `kkernel exec '<ops>'`."""

    def __init__(self, live: bool):
        self.live = live
        self.read_calls = 0
        self.write_calls = 0

    def _run(self, ops: str) -> dict:
        proc = subprocess.run(
            [KKERNEL, "exec", ops],
            capture_output=True,
            text=True,
            cwd=str(REPO_ROOT),
            timeout=60,
        )
        if proc.returncode != 0:
            raise RuntimeError(
                f"kkernel exec failed (rc={proc.returncode}): {proc.stderr}\nops={ops}"
            )
        try:
            return json.loads(proc.stdout)
        except json.JSONDecodeError as e:
            raise RuntimeError(f"kkernel exec returned non-JSON stdout: {proc.stdout[:500]}") from e

    def read(self, ops: str) -> dict:
        self.read_calls += 1
        return self._run(ops)

    def write(self, ops: str) -> dict:
        """Only ever called when self.live is True — callers must gate on it."""
        assert self.live, "write() called without --live"
        self.write_calls += 1
        return self._run(ops)


def first_op_result(resp: dict) -> dict:
    results = resp.get("results", [])
    if not results:
        raise RuntimeError(f"empty results in response: {resp}")
    op = results[0]
    if not op.get("ok"):
        raise RuntimeError(f"op failed: {op}")
    return op["result"]


# The server clamps `list` limit to 200 and, only when the caller's requested
# limit exceeded that clamp, wraps the array in
# {items, effective_limit, limit_clamped, requested_limit} instead of
# returning a bare array. Requesting <=200 always yields a bare array; we
# still normalize defensively in case that changes.
def list_items(result) -> list:
    if isinstance(result, dict) and "items" in result:
        return result["items"]
    return result


def fetch_all(kk: KKernel, kind: str, page_limit: int = 200, max_pages: int = 50) -> list[dict]:
    """Fetch every row of `kind` via bounded pagination. Read-only."""
    rows_all: list[dict] = []
    offset = 0
    for _ in range(max_pages):
        resp = kk.read(f"list(kind={dsl_str(kind)}, limit={page_limit}, offset={offset})")
        rows = list_items(first_op_result(resp))
        if not rows:
            break
        rows_all.extend(rows)
        if len(rows) < page_limit:
            break
        offset += page_limit
    return rows_all


def select_exact_project(project_rows: list[dict], expected_name: str) -> str | None:
    """Exact-name match against `project` entity rows. 0 or >1 matches -> None
    (an explicit miss the caller must log — never a fallback guess)."""
    exact = [r for r in project_rows if r.get("name") == expected_name]
    if len(exact) != 1:
        return None
    return exact[0]["id"]


def resolve_project_id(kk: KKernel, expected_name: str = WS_INGEST_PROJECT_NAME) -> str | None:
    project_id = select_exact_project(fetch_all(kk, "project"), expected_name)
    if project_id is None:
        print(
            f"  [project-resolve] could not uniquely resolve this repo's project "
            f"entity (expected exactly one `project` named {expected_name!r}); "
            "codex_verdict notes will NOT be annotated to any pull_request note "
            "this run.",
            file=sys.stderr,
        )
    return project_id


def filter_pr_by_number(pr_rows: list[dict], project_id: str | None) -> dict[int, str]:
    """number -> note id, restricted to pull_request notes whose
    properties.project_id matches this checkout's project. PR numbers are
    only unique within a project (mirrors khive-pack-git ingest.rs
    find_by_number); project_id=None (unresolved project) yields no matches
    rather than a number-only cross-repository fallback."""
    by_number: dict[int, str] = {}
    if project_id is None:
        return by_number
    for row in pr_rows:
        props = row.get("properties") or {}
        num = props.get("number")
        if isinstance(num, int) and props.get("project_id") == project_id:
            by_number[num] = row["id"]
    return by_number


def load_existing_workspaces(kk: KKernel) -> dict[str, str]:
    """lane name -> workspace entity id."""
    by_name: dict[str, str] = {}
    for row in fetch_all(kk, "workspace"):
        name = row.get("name")
        if name:
            by_name[name] = row["id"]
    return by_name


def load_existing_ws_ingest_notes(
    kk: KKernel, note_kinds: list[str], page_limit: int = 200, max_pages: int = 500
) -> dict[tuple[str, str], str]:
    """(source_path, content_sha256_16) -> note id, for notes already ingested
    by this script, across kinds. The note id is retained (not just the key)
    so a match can be reconciled — its `annotates` edges verified/backfilled
    — rather than accepted as complete on sight; see reconcile_existing_note."""
    seen: dict[tuple[str, str], str] = {}
    for kind in note_kinds:
        offset = 0
        for _ in range(max_pages):
            resp = kk.read(f"list(kind={dsl_str(kind)}, limit={page_limit}, offset={offset})")
            rows = list_items(first_op_result(resp))
            if not rows:
                break
            for row in rows:
                props = row.get("properties") or {}
                tags = props.get("tags") or row.get("tags") or []
                if INGEST_TAG not in tags:
                    continue
                sp = props.get("source_path")
                sha = props.get("content_sha256_16")
                if sp and sha:
                    seen[(sp, sha)] = row["id"]
            if len(rows) < page_limit:
                break
            offset += page_limit
    return seen


def fetch_outgoing_annotate_targets(kk: KKernel, note_id: str) -> set[str]:
    """Target ids of `note_id`'s outgoing `annotates` edges. Read-only."""
    resp = kk.read(
        f'neighbors(node_id={dsl_str(note_id)}, direction="outgoing", '
        f"relations={json.dumps(['annotates'])})"
    )
    hits = first_op_result(resp)
    return {hit["id"] for hit in hits if "id" in hit}


def compute_annotate_targets(
    a: Artifact, ws_id: str | None, pr_by_number: dict[int, str]
) -> list[str]:
    """The full set of `annotates` targets this artifact requires: its
    workspace lane entity, plus (for codex verdicts with a project-scoped PR
    match) the pull_request note. Pure — no I/O."""
    targets: list[str] = []
    if ws_id:
        targets.append(ws_id)
    if a.artifact_class == "codex_verdict" and a.pr_number in pr_by_number:
        targets.append(pr_by_number[a.pr_number])
    return targets


def missing_annotate_targets(required: list[str], existing: set[str]) -> list[str]:
    """Required targets not already covered by an existing outgoing edge.
    Pure — no I/O."""
    return [t for t in required if t not in existing]


def reconcile_existing_note(
    kk: KKernel,
    a: Artifact,
    note_id: str,
    ws_cache: dict[str, str],
    pr_by_number: dict[int, str],
    stats: Stats,
) -> None:
    """A note matching this artifact's (source_path, sha16) key was found by
    the startup server scan. Before accepting it as complete, verify its
    outgoing `annotates` edges cover every target this artifact requires and
    create whatever is missing — a note can be present with a missing edge
    if it was written by a pre-fix run, or if create-time link-failure
    compensation (best-effort, ignores its own failures — see
    crates/khive-runtime/src/operations.rs) did not run to completion."""
    ws_id = ensure_workspace(kk, a.lane, ws_cache, live=True, stats=stats)
    required = compute_annotate_targets(a, ws_id, pr_by_number)
    if not required:
        return
    existing_targets = fetch_outgoing_annotate_targets(kk, note_id)
    for target_id in missing_annotate_targets(required, existing_targets):
        resp = kk.write(
            f"link(source_id={dsl_str(note_id)}, target_id={dsl_str(target_id)}, "
            f'relation="annotates")'
        )
        first_op_result(resp)
        stats.edges_backfilled += 1


def cap_content(raw: bytes) -> str:
    """Decode `raw` (replacing invalid UTF-8) and cap the FINAL encoded
    payload at MAX_CONTENT_BYTES, marker space reserved. Capping on the
    original raw byte length is not sufficient: `errors="replace"` can
    expand invalid bytes (each -> U+FFFD, 3 bytes), so the cap must apply
    after replacement decoding, not before."""
    text = raw.decode("utf-8", errors="replace")
    encoded = text.encode("utf-8")
    if len(encoded) <= MAX_CONTENT_BYTES:
        return text
    marker = TRUNCATION_MARKER.format(orig=len(raw))
    budget = max(MAX_CONTENT_BYTES - len(marker.encode("utf-8")), 0)
    return encoded[:budget].decode("utf-8", errors="ignore") + marker


def build_note_content(artifact: Artifact) -> str:
    return cap_content(artifact.path.read_bytes())


def ensure_workspace(
    kk: KKernel, lane: str, cache: dict[str, str], live: bool, stats: Stats
) -> str | None:
    if lane in cache:
        return cache[lane]
    stats.workspaces_to_create += 1
    if not live:
        return None
    ws_props = json.dumps({"schema_version": 1, "lane": lane, "created_by": "ws-ingest"})
    resp = kk.write(
        f"create(kind={dsl_str('workspace')}, name={dsl_str(lane)}, "
        f"properties={ws_props}, tags={json.dumps([INGEST_TAG])})"
    )
    ws_id = first_op_result(resp)["id"]
    cache[lane] = ws_id
    return ws_id


@dataclass
class Stats:
    by_class: dict = field(default_factory=lambda: dict.fromkeys(NOTE_KIND, 0))
    notes_created: int = 0
    notes_skipped_existing: int = 0
    notes_skipped_cursor: int = 0
    workspaces_to_create: int = 0
    edges_workspace_planned: int = 0
    edges_pr_planned: int = 0
    edges_backfilled: int = 0
    pr_link_hits: int = 0
    pr_link_misses: int = 0
    pr_link_na: int = 0
    samples: list = field(default_factory=list)
    blocked_secret_gate: list = field(default_factory=list)


def _acquire_live_lock() -> "object":
    """Machine-wide exclusion for --live runs (fcntl.flock, non-blocking, on
    a fixed path shared by every checkout on this host). See module
    docstring CONCURRENCY note for what this does and does not cover."""
    fd = LOCK_PATH.open("w")
    try:
        fcntl.flock(fd.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
    except OSError as e:
        fd.close()
        raise RuntimeError(
            f"another --live ws-ingest run already holds {LOCK_PATH} (machine-wide "
            "exclusion lock, shared across checkouts) — wait for it to finish "
            "before retrying. This lock does NOT exclude concurrent --live runs "
            "on a different host against the same khive.db; see module docstring "
            "CONCURRENCY note."
        ) from e
    return fd


def process(dry_run: bool, only_classes: set[str] | None = None) -> tuple[Stats, list[Artifact]]:
    live = not dry_run
    kk = KKernel(live=live)
    stats = Stats()
    lock_fd = _acquire_live_lock() if live else None

    try:
        artifacts = discover()
        if only_classes:
            artifacts = [a for a in artifacts if a.artifact_class in only_classes]
        for a in artifacts:
            stats.by_class[a.artifact_class] += 1

        cursor_done = load_cursor()

        ws_cache = load_existing_workspaces(kk)
        project_id = resolve_project_id(kk)
        pr_by_number = filter_pr_by_number(fetch_all(kk, "pull_request"), project_id)
        existing_notes = (
            load_existing_ws_ingest_notes(kk, sorted(set(NOTE_KIND.values()))) if live else {}
        )

        lanes_needed = sorted({a.lane for a in artifacts})
        for lane in lanes_needed:
            if lane not in ws_cache:
                stats.workspaces_to_create += 1

        for a in artifacts:
            _process_one(kk, a, live, ws_cache, pr_by_number, cursor_done, existing_notes, stats)
    finally:
        if lock_fd is not None:
            fcntl.flock(lock_fd.fileno(), fcntl.LOCK_UN)
            lock_fd.close()

    return stats, artifacts


def _process_one(
    kk: KKernel,
    a: Artifact,
    live: bool,
    ws_cache: dict[str, str],
    pr_by_number: dict[int, str],
    cursor_done: set[tuple[str, str]],
    existing_notes: dict[tuple[str, str], str],
    stats: Stats,
) -> None:
    key = (a.rel_path, a.sha16)

    if a.artifact_class == "codex_verdict":
        if a.pr_number is None:
            stats.pr_link_na += 1
        elif a.pr_number in pr_by_number:
            stats.pr_link_hits += 1
        else:
            stats.pr_link_misses += 1

    if key in cursor_done:
        stats.notes_skipped_cursor += 1
        return
    if live and key in existing_notes:
        reconcile_existing_note(kk, a, existing_notes[key], ws_cache, pr_by_number, stats)
        stats.notes_skipped_existing += 1
        append_cursor(*key)
        return

    note_kind = NOTE_KIND[a.artifact_class]
    note_tags = [INGEST_TAG, a.artifact_class]
    stats.edges_workspace_planned += 1
    if a.artifact_class == "codex_verdict" and a.pr_number in pr_by_number:
        stats.edges_pr_planned += 1

    if len(stats.samples) < 10:
        stats.samples.append(
            {
                "source_path": a.rel_path,
                "artifact_class": a.artifact_class,
                "note_kind": note_kind,
                "lane": a.lane,
                "pr_number": a.pr_number,
                "pr_link": (
                    "hit"
                    if (a.pr_number is not None and a.pr_number in pr_by_number)
                    else ("miss" if a.pr_number is not None else "n/a")
                ),
                "sha16": a.sha16,
                "size": a.size,
            }
        )

    if not live:
        stats.notes_created += 1  # "would create"
        return

    ws_id = ensure_workspace(kk, a.lane, ws_cache, live, stats)

    content = build_note_content(a)
    properties = {
        "source_path": a.rel_path,
        "content_sha256_16": a.sha16,
        "artifact_class": a.artifact_class,
        "ingested_at": datetime.now(UTC).isoformat(),
    }
    if a.pr_number is not None:
        properties["pr_number"] = a.pr_number

    # Note + its annotates edges are created atomically in one call: the
    # runtime validates every target exists before any write and compensates
    # (deletes the note row) if an edge fails — see module docstring.
    annotate_targets = compute_annotate_targets(a, ws_id, pr_by_number)
    if (
        a.artifact_class == "codex_verdict"
        and a.pr_number is not None
        and a.pr_number not in pr_by_number
    ):
        print(
            f"  [pr-link miss] {a.rel_path}: no project-scoped pull_request "
            f"note for #{a.pr_number}",
            file=sys.stderr,
        )

    name = a.rel_path.rsplit("/", 1)[-1][:120]
    props_json = json.dumps(properties)
    resp = kk.write(
        f"create(kind={dsl_str(note_kind)}, name={dsl_str(name)}, "
        f"content={dsl_str(content)}, properties={props_json}, "
        f"tags={json.dumps(note_tags)}, annotates={json.dumps(annotate_targets)})"
    )
    try:
        first_op_result(resp)  # raises on failure; note+edges are all-or-nothing
    except RuntimeError as e:
        if "matches secret pattern" in str(e):
            # Daemon secret-gate false positive on legitimate artifact text
            # (high-entropy string near a trigger word). Content is never
            # reworded to dodge the gate — the artifact stays un-cursored
            # for a later pass once the gate's masking handles it.
            stats.blocked_secret_gate.append(a.rel_path)
            print(f"  [secret-gate blocked] {a.rel_path}", file=sys.stderr)
            return
        raise

    stats.notes_created += 1
    append_cursor(*key)


def write_dryrun_report(stats: Stats, artifacts: list[Artifact], out_path: Path) -> None:
    lines = []
    lines.append("# Workspace-artifact ingest — DRY RUN")
    lines.append("")
    lines.append(f"Generated: {datetime.now(UTC).isoformat()}")
    lines.append(f"Total files discovered: {len(artifacts)}")
    lines.append("")
    lines.append("## Counts by artifact_class")
    lines.append("")
    lines.append("| artifact_class | note_kind | file count |")
    lines.append("|---|---|---|")
    for k, v in stats.by_class.items():
        lines.append(f"| {k} | {NOTE_KIND[k]} | {v} |")
    lines.append("")
    lines.append("## Planned writes")
    lines.append("")
    lines.append(f"- Workspace entities to create: {stats.workspaces_to_create}")
    lines.append(f"- Notes to create: {stats.notes_created}")
    lines.append(f"- Notes skipped (already in cursor): {stats.notes_skipped_cursor}")
    lines.append(
        f"- Notes skipped (found on server, cursor backfilled): {stats.notes_skipped_existing}"
    )
    lines.append(f"- annotates edges to workspace entity planned: {stats.edges_workspace_planned}")
    lines.append(f"- annotates edges to pull_request note planned: {stats.edges_pr_planned}")
    lines.append("")
    lines.append("## PR-link hit/miss (codex_verdict files only)")
    lines.append("")
    lines.append(f"- hit (matching pull_request note found): {stats.pr_link_hits}")
    lines.append(f"- miss (no pull_request note for that number): {stats.pr_link_misses}")
    lines.append(f"- n/a (filename carried no parseable PR number): {stats.pr_link_na}")
    lines.append("")
    lines.append("## Sample mappings (first 10 planned)")
    lines.append("")
    lines.append(
        "| source_path | artifact_class | note_kind | lane | pr_number | pr_link | sha16 | size |"
    )
    lines.append("|---|---|---|---|---|---|---|---|")
    for s in stats.samples:
        lines.append(
            f"| {s['source_path']} | {s['artifact_class']} | {s['note_kind']} | {s['lane']} | "
            f"{s['pr_number']} | {s['pr_link']} | {s['sha16']} | {s['size']} |"
        )
    lines.append("")
    out_path.write_text("\n".join(lines) + "\n")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--live", action="store_true", help="Perform real khive writes (default: dry-run)"
    )
    parser.add_argument(
        "--report",
        type=Path,
        default=None,
        help="Path to write the dry-run markdown report (dry-run mode only)",
    )
    parser.add_argument(
        "--only-class",
        action="append",
        choices=sorted(NOTE_KIND.keys()),
        default=None,
        help="Restrict this run to the given artifact class(es) — the tranche knob "
        "(repeatable). Cursor idempotency makes later full runs skip these.",
    )
    args = parser.parse_args()

    dry_run = not args.live
    only_classes = set(args.only_class) if args.only_class else None
    stats, artifacts = process(dry_run=dry_run, only_classes=only_classes)

    if dry_run:
        report_path = args.report or (
            KHIVE_DIR / "workspaces" / "20260716" / "ws-artifact-ingest" / "DRYRUN.md"
        )
        report_path.parent.mkdir(parents=True, exist_ok=True)
        write_dryrun_report(stats, artifacts, report_path)
        print(f"DRY RUN complete. Report: {report_path}")
    else:
        print(
            f"LIVE run complete. notes_created={stats.notes_created} "
            f"skipped_cursor={stats.notes_skipped_cursor} "
            f"skipped_existing={stats.notes_skipped_existing} "
            f"edges_backfilled={stats.edges_backfilled} "
            f"blocked_secret_gate={len(stats.blocked_secret_gate)}"
        )
        for rel in stats.blocked_secret_gate:
            print(f"  blocked: {rel}")

    print(json.dumps(stats.by_class, indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main())
