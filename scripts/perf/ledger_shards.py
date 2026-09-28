#!/usr/bin/env python3
"""Bounded, append-ordered component trend-ledger shards on ``perf-data``.

The historical ``components.jsonl`` is migrated on the first publish. New
records go into numbered segments, so no new Git blob approaches GitHub's
100 MB file limit. The main checkout still writes its small, run-local
``components.jsonl``; only the publisher changes the remote layout.
"""

from __future__ import annotations

import argparse
import json
import pathlib
import re
import sys
from collections.abc import Iterable, Iterator

MAX_COMPONENT_SHARD_BYTES = 32 * 1024 * 1024
_SHARD_NAME = re.compile(r"[0-9]{6}\.jsonl\Z")


def component_shard_paths(data_dir: pathlib.Path) -> list[pathlib.Path]:
    shard_dir = data_dir / "components"
    if not shard_dir.exists():
        return []
    paths = sorted(shard_dir.glob("*.jsonl"))
    unknown = [path.name for path in paths if not _SHARD_NAME.fullmatch(path.name) or not path.is_file()]
    if unknown:
        raise ValueError(f"unrecognized component ledger shard(s): {unknown}")
    return paths


def component_ledger_paths(data_dir: pathlib.Path) -> list[pathlib.Path]:
    """Read in append order, including a legacy file restored by an old job.

    Before the migration there are only flat-file records. After it, shards
    contain the history; an old concurrent publisher can reintroduce a small
    flat file, which is newer and follows those shards until the next publish
    migrates it too.
    """
    shards = component_shard_paths(data_dir)
    legacy = data_dir / "components.jsonl"
    return shards + ([legacy] if legacy.is_file() else [])


def _lines(path: pathlib.Path) -> Iterator[bytes]:
    with path.open("rb") as stream:
        for raw in stream:
            line = raw.rstrip(b"\r\n")
            if line:
                yield line + b"\n"


def _append_lines(data_dir: pathlib.Path, lines: Iterable[bytes], max_shard_bytes: int) -> int:
    shards = component_shard_paths(data_dir)
    index = int(shards[-1].stem) if shards else 1
    size = shards[-1].stat().st_size if shards else 0
    added = 0
    for line in lines:
        if len(line) > max_shard_bytes:
            raise ValueError(f"one component record exceeds the {max_shard_bytes}-byte shard cap")
        if size and size + len(line) > max_shard_bytes:
            index += 1
            size = 0
        if index > 999999:
            raise ValueError("component shard sequence exhausted")
        target = data_dir / "components" / f"{index:06d}.jsonl"
        target.parent.mkdir(parents=True, exist_ok=True)
        with target.open("ab") as stream:
            stream.write(line)
        size += len(line)
        added += 1
    return added


def merge_components(
    source: pathlib.Path,
    data_dir: pathlib.Path,
    *,
    max_shard_bytes: int = MAX_COMPONENT_SHARD_BYTES,
) -> tuple[int, int]:
    """Migrate any flat history, then append source lines absent from all shards.

    This is rerun from the fetched branch on every optimistic push attempt.
    Exact-line dedup covers the legacy file, every shard, and retry/rerun
    input without loading the growing history into memory at once.
    """
    if max_shard_bytes <= 0:
        raise ValueError("shard cap must be positive")
    incoming = dict.fromkeys(_lines(source))
    for line in incoming:
        record = json.loads(line)
        if not isinstance(record, dict) or record.get("suite") != "components":
            raise ValueError("component ledger input must contain component records")

    data_dir.mkdir(parents=True, exist_ok=True)
    legacy = data_dir / "components.jsonl"
    migrated = 0
    if legacy.is_file():
        shards = component_shard_paths(data_dir)
        if shards:
            # Only an old, concurrent publisher should leave both layouts.
            # Its restored flat file is small; skip lines already in shards.
            pending_legacy = dict.fromkeys(_lines(legacy))
            for shard in shards:
                for line in _lines(shard):
                    pending_legacy.pop(line, None)
            migrated = _append_lines(data_dir, pending_legacy, max_shard_bytes)
        else:
            # First migration streams the existing 76 MB ledger into bounded
            # blobs without assembling a second in-memory copy of it.
            migrated = _append_lines(data_dir, _lines(legacy), max_shard_bytes)
        legacy.unlink()

    for shard in component_shard_paths(data_dir):
        for line in _lines(shard):
            incoming.pop(line, None)
    appended = _append_lines(data_dir, incoming, max_shard_bytes)
    return migrated, appended


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=pathlib.Path)
    parser.add_argument("data_dir", type=pathlib.Path)
    args = parser.parse_args(argv)
    try:
        migrated, appended = merge_components(args.source, args.data_dir)
    except (OSError, ValueError, json.JSONDecodeError) as error:
        print(f"[ledger_shards] component publish failed: {error}", file=sys.stderr)
        return 1
    print(f"[ledger_shards] migrated={migrated} appended={appended}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
