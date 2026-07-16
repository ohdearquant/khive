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
  bulk-loads existing ws-ingest-tagged notes once at startup and skips any
  file whose key is already present there.
- Content is truncated at ~48KB with a trailing marker; the sha256 is always
  computed over the ORIGINAL (untruncated) bytes so the dedup key is stable.
"""

from __future__ import annotations

import argparse
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
KKERNEL = os.path.expanduser("~/.cargo/bin/kkernel")

MAX_CONTENT_BYTES = 48 * 1024
TRUNCATION_MARKER = "\n\n...[truncated by ws-ingest, original length {orig} bytes]...\n"

INGEST_TAG = "ws-ingest"

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
    """Escape a string for embedding as a double-quoted DSL literal."""
    return s.replace("\\", "\\\\").replace('"', '\\"').replace("\n", "\\n").replace("\r", "")


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


def annotate_op(source_id: str, target_id: str) -> str:
    src, tgt, rel = dsl_str(source_id), dsl_str(target_id), dsl_str("annotates")
    return f"link(source_id={src}, target_id={tgt}, relation={rel})"


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


def load_existing_pull_requests(
    kk: KKernel, page_limit: int = 200, max_pages: int = 50
) -> dict[int, str]:
    """number -> note id, for existing pull_request notes. Bounded pagination."""
    by_number: dict[int, str] = {}
    offset = 0
    for _ in range(max_pages):
        resp = kk.read(f"list(kind={dsl_str('pull_request')}, limit={page_limit}, offset={offset})")
        rows = list_items(first_op_result(resp))
        if not rows:
            break
        for row in rows:
            props = row.get("properties") or {}
            num = props.get("number")
            if isinstance(num, int):
                by_number[num] = row["id"]
        if len(rows) < page_limit:
            break
        offset += page_limit
    return by_number


def load_existing_workspaces(
    kk: KKernel, page_limit: int = 200, max_pages: int = 50
) -> dict[str, str]:
    """lane name -> workspace entity id."""
    by_name: dict[str, str] = {}
    offset = 0
    for _ in range(max_pages):
        resp = kk.read(f"list(kind={dsl_str('workspace')}, limit={page_limit}, offset={offset})")
        rows = list_items(first_op_result(resp))
        if not rows:
            break
        for row in rows:
            name = row.get("name")
            if name:
                by_name[name] = row["id"]
        if len(rows) < page_limit:
            break
        offset += page_limit
    return by_name


def load_existing_ws_ingest_notes(
    kk: KKernel, note_kinds: list[str], page_limit: int = 200, max_pages: int = 500
) -> set[tuple[str, str]]:
    """(source_path, content_sha256_16) pairs already ingested by this script, across kinds."""
    seen: set[tuple[str, str]] = set()
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
                    seen.add((sp, sha))
            if len(rows) < page_limit:
                break
            offset += page_limit
    return seen


def build_note_content(artifact: Artifact) -> str:
    raw = artifact.path.read_bytes()
    text = raw.decode("utf-8", errors="replace")
    if len(raw) > MAX_CONTENT_BYTES:
        text = text.encode("utf-8", errors="replace")[:MAX_CONTENT_BYTES].decode(
            "utf-8", errors="ignore"
        )
        text += TRUNCATION_MARKER.format(orig=len(raw))
    return text


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
    pr_link_hits: int = 0
    pr_link_misses: int = 0
    pr_link_na: int = 0
    samples: list = field(default_factory=list)


def process(dry_run: bool) -> tuple[Stats, list[Artifact]]:
    live = not dry_run
    kk = KKernel(live=live)
    stats = Stats()

    artifacts = discover()
    for a in artifacts:
        stats.by_class[a.artifact_class] += 1

    cursor_done = load_cursor()

    ws_cache = load_existing_workspaces(kk)
    pr_by_number = load_existing_pull_requests(kk)
    existing_notes = (
        load_existing_ws_ingest_notes(kk, sorted(set(NOTE_KIND.values()))) if live else set()
    )

    lanes_needed = sorted({a.lane for a in artifacts})
    for lane in lanes_needed:
        if lane not in ws_cache:
            stats.workspaces_to_create += 1

    for a in artifacts:
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
            continue
        if live and key in existing_notes:
            stats.notes_skipped_existing += 1
            append_cursor(*key)
            continue

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
            continue

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

        name = a.rel_path.rsplit("/", 1)[-1][:120]
        props_json = json.dumps(properties)
        resp = kk.write(
            f"create(kind={dsl_str(note_kind)}, name={dsl_str(name)}, "
            f"content={dsl_str(content)}, properties={props_json}, tags={json.dumps(note_tags)})"
        )
        note_id = first_op_result(resp)["id"]

        if ws_id:
            kk.write(annotate_op(note_id, ws_id))

        if a.artifact_class == "codex_verdict" and a.pr_number in pr_by_number:
            kk.write(annotate_op(note_id, pr_by_number[a.pr_number]))
        elif a.artifact_class == "codex_verdict" and a.pr_number is not None:
            print(
                f"  [pr-link miss] {a.rel_path}: no pull_request note for #{a.pr_number}",
                file=sys.stderr,
            )

        stats.notes_created += 1
        append_cursor(*key)

    return stats, artifacts


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
    args = parser.parse_args()

    dry_run = not args.live
    stats, artifacts = process(dry_run=dry_run)

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
            f"skipped_existing={stats.notes_skipped_existing}"
        )

    print(json.dumps(stats.by_class, indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main())
