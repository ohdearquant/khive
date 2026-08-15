#!/usr/bin/env python3
"""Deterministic 400-note synthetic corpus generator for the retrieval eval harness.

Produces the note set the #939 baseline protocol describes: 40 proper-noun/
code-bearing targets across 4 topic clusters (10 each), 32 same-topic generic
distractors (8 per cluster), and 328 unrelated background notes. Note keys
match the keys used in queries.jsonl's per-query `labels`/`target_keys`/
`same_topic_generic_keys` fields.

Target content is derived from the query text in queries.jsonl (not invented
independently), so each target note actually answers the query that names it.

Deterministic: a fixed `--seed` drives every randomized choice (age jitter,
salience jitter, background-note sampling) and a fixed `--epoch` (ISO-8601,
default 2026-08-01T00:00:00Z) anchors every note's age. No wall-clock read.

Usage:
    uv run python generate_corpus.py --queries queries.jsonl --out-dir corpus
"""

from __future__ import annotations

import argparse
import json
import random
import re
from datetime import datetime, timedelta
from pathlib import Path

DEFAULT_SEED = 939_400
DEFAULT_EPOCH = "2026-08-01T00:00:00Z"

EXACT_RE = re.compile(
    r"^What (?P<topic>.+) instruction applies to (?P<a>.+) and (?P<b>.+)\?$"
)
PARA_RE = re.compile(
    r"^Find the (?P<action>.+) guidance owned by (?P<person>.+) for the (?P<case>.+) case\.$"
)
FRESH_RE = re.compile(
    r"^Latest directive for (?P<code>.+): which (?P<topic>.+) marker and owner must be retained\?$"
)

FRESH_OWNER_POOL = [
    "Vik Osei",
    "Priya Nair",
    "Owen Cruz",
    "Dana Frost",
    "Kai Marsh",
    "Rosa Feld",
    "Theo Lang",
    "Nia Brooks",
    "Ivo Santos",
    "Wren Foley",
]

GENERIC_VARIANTS = [
    "This reference summarizes the standing approach without naming a specific case.",
    "This note captures the general checklist maintained for the wider team.",
    "This record lists baseline expectations shared across similar cases.",
    "This entry documents the default process absent case-specific detail.",
    "This memo restates the common playbook used before case triage.",
    "This page holds the boilerplate procedure pending case assignment.",
    "This draft covers the shared prerequisites ahead of case-specific work.",
    "This summary tracks the recurring pattern observed across past cases.",
]

BACKGROUND_TOPICS = [
    "quarterly office supply inventory reconciliation",
    "team offsite venue shortlist and catering notes",
    "internal wiki migration checklist",
    "coffee machine maintenance schedule",
    "conference room booking etiquette reminder",
    "onboarding buddy pairing rotation",
    "desk plant watering rota",
    "printer toner reorder threshold",
    "shared calendar color-coding convention",
    "parking permit renewal steps",
    "book club reading list for the quarter",
    "hallway whiteboard cleaning schedule",
    "guest wifi password rotation policy",
    "lunch-and-learn topic backlog",
    "recycling bin sorting guide",
    "building badge access renewal process",
    "shared drive folder naming convention",
    "meeting room AV troubleshooting tips",
    "birthday celebration snack budget",
    "commute survey summary for the quarter",
    "bike rack expansion proposal notes",
    "internal newsletter submission deadline",
    "standing desk request queue",
    "software license renewal tracker",
    "travel expense reimbursement reminder",
    "holiday schedule for the regional office",
    "mentorship program pairing notes",
    "kitchen cleanup rota for the floor",
    "visitor sign-in process update",
    "archive retention policy for old notes",
]

CLUSTERS = ["release", "ledger", "research", "support"]


def parse_queries(path: Path) -> list[dict]:
    rows = []
    with path.open() as f:
        for line in f:
            line = line.strip()
            if line:
                rows.append(json.loads(line))
    return rows


def derive_cluster_meta(queries: list[dict]) -> dict[str, dict]:
    meta: dict[str, dict] = {c: {} for c in CLUSTERS}
    for q in queries:
        cluster = q["cluster"]
        m = EXACT_RE.match(q["query"])
        if m:
            meta[cluster]["topic"] = m.group("topic")
        p = PARA_RE.match(q["query"])
        if p:
            meta[cluster]["case_phrase"] = p.group("case")
        fr = FRESH_RE.match(q["query"])
        if fr:
            meta[cluster].setdefault("topic", fr.group("topic"))
    for cluster, m in meta.items():
        if "topic" not in m or "case_phrase" not in m:
            raise ValueError(
                f"could not derive topic/case_phrase for cluster {cluster!r}: {m}"
            )
    return meta


def target_content(query: dict, cluster_meta: dict, rng: random.Random) -> str:
    q = query["query"]
    cluster = query["cluster"]
    topic = cluster_meta[cluster]["topic"]
    case_phrase = cluster_meta[cluster]["case_phrase"]
    idx = int(query["target_keys"][0].rsplit("_", 1)[-1])

    m = EXACT_RE.match(q)
    if m:
        a, b = m.group("a"), m.group("b")
        return (
            f"{topic.capitalize()} of record for {a} and {b}: confirm the {a} marker before "
            f"initiating recovery, then verify {b} is restored to its last verified state. "
            f"This is the governing instruction for the {case_phrase} and must be followed "
            f"exactly; do not substitute an unlisted marker or owner."
        )

    p = PARA_RE.match(q)
    if p:
        person, action = p.group("person"), p.group("action")
        return (
            f"{person} is the owner of record for the {case_phrase}. The current written "
            f"procedure covers what to do when the primary signal degrades: capture the "
            f"incident context, apply the standing recovery checklist, and confirm the "
            f"condition clears before sign-off. This guidance ({action}) supersedes any "
            f"undocumented verbal instruction for the same case."
        )

    fr = FRESH_RE.match(q)
    if fr:
        code = fr.group("code")
        owner = FRESH_OWNER_POOL[(idx - 1) % len(FRESH_OWNER_POOL)]
        marker = f"DR-{cluster.upper()}-{idx:02d}"
        return (
            f"Directive log entry for {code}: the current {topic} marker is {marker}; "
            f"owner of record is {owner}. This entry supersedes any earlier marker recorded "
            f"for {code} in the {case_phrase} log and must be retained until the next "
            f"directive is issued."
        )

    raise ValueError(f"query does not match any known template: {q!r}")


def generic_content(cluster: str, cluster_meta: dict, gid: int) -> str:
    topic = cluster_meta[cluster]["topic"]
    case_phrase = cluster_meta[cluster]["case_phrase"]
    variant = GENERIC_VARIANTS[(gid - 1) % len(GENERIC_VARIANTS)]
    return (
        f"General {topic} reference for the {case_phrase}: this record outlines the "
        f"standard approach used across similar cases in this area, without naming a "
        f"specific ticket, code, or owner. {variant} Refer to the case-specific directive "
        f"for the applicable marker before acting."
    )


def background_content(bid: int, rng: random.Random) -> str:
    topic = BACKGROUND_TOPICS[(bid - 1) % len(BACKGROUND_TOPICS)]
    variant_no = 1 + ((bid - 1) // len(BACKGROUND_TOPICS))
    return (
        f"Background note #{bid:03d} (revision {variant_no}): {topic}. This is routine "
        f"operational context unrelated to any active incident, release, ledger, or "
        f"research case; no action is required unless separately flagged."
    )


def build_corpus(queries_path: Path, seed: int, epoch_iso: str) -> list[dict]:
    queries = parse_queries(queries_path)
    cluster_meta = derive_cluster_meta(queries)
    epoch = datetime.fromisoformat(epoch_iso.replace("Z", "+00:00"))
    rng = random.Random(seed)

    notes: dict[str, dict] = {}

    target_queries = {
        q["target_keys"][0]: q for q in queries if len(q["target_keys"]) == 1
    }
    for key, q in sorted(target_queries.items()):
        content = target_content(q, cluster_meta, rng)
        is_fresh = q["query_class"] == "fresh_directive"
        age_days = rng.randint(1, 5) if is_fresh else rng.randint(10, 90)
        salience = round(rng.uniform(0.55, 0.85), 3)
        notes[key] = {
            "key": key,
            "content": content,
            "salience": salience,
            "decay_factor": 0.0,
            "memory_type": "semantic",
            "age_days": age_days,
        }

    generic_keys: set[str] = set()
    for q in queries:
        generic_keys.update(q["same_topic_generic_keys"])
    for key in sorted(generic_keys):
        cluster, gid_s = key.split("_generic_")
        gid = int(gid_s)
        content = generic_content(cluster, cluster_meta, gid)
        age_days = rng.randint(5, 150)
        salience = round(rng.uniform(0.35, 0.6), 3)
        notes[key] = {
            "key": key,
            "content": content,
            "salience": salience,
            "decay_factor": 0.0,
            "memory_type": "semantic",
            "age_days": age_days,
        }

    any_labels = queries[0]["labels"]
    background_keys = sorted(k for k in any_labels if k.startswith("background_"))
    for key in background_keys:
        bid = int(key.rsplit("_", 1)[-1])
        content = background_content(bid, rng)
        age_days = rng.randint(1, 400)
        salience = round(rng.uniform(0.1, 0.5), 3)
        notes[key] = {
            "key": key,
            "content": content,
            "salience": salience,
            "decay_factor": 0.0,
            "memory_type": "semantic",
            "age_days": age_days,
        }

    ordered = [notes[k] for k in sorted(notes.keys())]
    for n in ordered:
        n["created_at_iso"] = (
            (epoch - timedelta(days=n["age_days"])).isoformat().replace("+00:00", "Z")
        )
    return ordered


def write_outputs(notes: list[dict], out_dir: Path) -> None:
    out_dir.mkdir(parents=True, exist_ok=True)
    notes_path = out_dir / "notes.jsonl"
    ops_path = out_dir / "seed_ops.jsonl"
    with notes_path.open("w") as nf, ops_path.open("w") as of:
        for n in notes:
            nf.write(json.dumps(n, sort_keys=True) + "\n")
            op = {
                "tool": "memory.remember",
                "args": {
                    "content": n["content"],
                    "salience": n["salience"],
                    "decay_factor": n["decay_factor"],
                    "memory_type": n["memory_type"],
                    "tags": [f"k:{n['key']}"],
                },
            }
            of.write(json.dumps(op, sort_keys=True) + "\n")


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument(
        "--queries", type=Path, default=Path(__file__).parent / "queries.jsonl"
    )
    ap.add_argument("--out-dir", type=Path, default=Path(__file__).parent / "corpus")
    ap.add_argument("--seed", type=int, default=DEFAULT_SEED)
    ap.add_argument("--epoch", type=str, default=DEFAULT_EPOCH)
    args = ap.parse_args()

    notes = build_corpus(args.queries, args.seed, args.epoch)
    if len(notes) != 400:
        raise SystemExit(f"expected 400 notes, built {len(notes)}")
    write_outputs(notes, args.out_dir)
    print(f"wrote {len(notes)} notes to {args.out_dir}")


if __name__ == "__main__":
    main()
