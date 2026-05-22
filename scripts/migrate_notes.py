#!/usr/bin/env python3
"""Migrate notes from internal khive DB to OSS khive-graph DB.

Copies all live (non-deleted) notes from the internal backup into the OSS
substrate. Populates the FTS index. Vector embeddings are left to the
runtime (generated on first search).

Usage:
    uv run python scripts/migrate_notes.py <source.db> [--dry-run]
    uv run python scripts/migrate_notes.py ~/.khive/khive.db.backup --dry-run
"""
import json
import sqlite3
import sys
from pathlib import Path

OSS_DB = Path.home() / ".khive" / "khive-graph.db"


def migrate(source_db: Path, dry_run: bool = False):
    if not source_db.exists():
        print(f"ERROR: source DB not found at {source_db}")
        sys.exit(1)
    if not OSS_DB.exists():
        print(f"ERROR: OSS DB not found at {OSS_DB}")
        sys.exit(1)

    src = sqlite3.connect(str(source_db))
    dst = sqlite3.connect(str(OSS_DB))
    dst.execute("PRAGMA journal_mode=WAL")
    dst.execute("PRAGMA foreign_keys=OFF")

    src_cursor = src.execute(
        "SELECT id, namespace, kind, content, salience, decay_factor, "
        "expires_at, properties, created_at, updated_at "
        "FROM notes WHERE deleted_at IS NULL"
    )

    inserted = 0
    skipped = 0
    by_kind: dict[str, int] = {}

    for row in src_cursor:
        note_id, namespace, kind, content, salience, decay_factor, \
            expires_at, properties, created_at, updated_at = row

        existing = dst.execute(
            "SELECT 1 FROM notes WHERE id = ?", (note_id,)
        ).fetchone()
        if existing:
            skipped += 1
            continue

        if dry_run:
            inserted += 1
            by_kind[kind] = by_kind.get(kind, 0) + 1
            continue

        dst.execute(
            "INSERT INTO notes (id, namespace, kind, name, content, salience, "
            "decay_factor, expires_at, properties, created_at, updated_at) "
            "VALUES (?, ?, ?, NULL, ?, ?, ?, ?, ?, ?, ?)",
            (note_id, namespace, kind, content, salience, decay_factor,
             expires_at, properties, created_at, updated_at),
        )

        props = {}
        if properties:
            try:
                props = json.loads(properties)
            except (json.JSONDecodeError, TypeError):
                pass

        tags_str = ",".join(props.get("tags", [])) if isinstance(props.get("tags"), list) else ""
        title = ""
        body = content or ""

        dst.execute(
            "INSERT INTO fts_notes_local (subject_id, kind, title, body, tags, "
            "namespace, metadata, updated_at) "
            "VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            (note_id, kind, title, body, tags_str, namespace, properties or "", updated_at),
        )

        inserted += 1
        by_kind[kind] = by_kind.get(kind, 0) + 1

    if not dry_run:
        dst.commit()

    src.close()
    dst.close()

    mode = "DRY RUN" if dry_run else "MIGRATED"
    print(f"\n{mode}: {inserted} notes inserted, {skipped} skipped (already exist)")
    print("By kind:")
    for kind, count in sorted(by_kind.items(), key=lambda x: -x[1]):
        print(f"  {kind}: {count}")


if __name__ == "__main__":
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    if not args:
        print("Usage: uv run python scripts/migrate_notes.py <source.db> [--dry-run]")
        sys.exit(1)
    dry_run = "--dry-run" in sys.argv
    migrate(Path(args[0]), dry_run=dry_run)
