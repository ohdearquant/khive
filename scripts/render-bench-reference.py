#!/usr/bin/env python3
"""Render committed benchmark references into crate benchmark ledgers.

The input is ``crates/<crate>/perf/reference.json``.  The output is the block
between ``<!-- bench-reference:begin -->`` and ``<!-- bench-reference:end -->``
in that crate's ``docs/benchmarks.md``.  Everything outside the block is left
alone.

The renderer uses only the Python standard library.  It reads the reference
file as JSON and asks git for the committer date of each baseline commit.  It
does not run a benchmark, use the clock, or write a timestamp of its own.
``--check`` reports every stale document and exits 1.  ``--write`` rewrites the
stale documents.
"""

from __future__ import annotations

import argparse
import json
import pathlib
import subprocess
import sys
from typing import Any, Iterable, NamedTuple


BEGIN_MARKER = "<!-- bench-reference:begin -->"
END_MARKER = "<!-- bench-reference:end -->"
GENERATED_NOTICE = (
    "Generated from `perf/reference.json` by "
    "`scripts/render-bench-reference.py`; do not edit this block."
)
UNMEASURED = "unmeasured"
NOT_A_TIMING = "n/a"


class RenderError(Exception):
    """A reference file that cannot be rendered without inventing evidence."""


class Row(NamedTuple):
    benchmark: str
    value: str
    aa_spread: str
    commit: str
    date: str


class Entry(NamedTuple):
    commit: str
    date: str
    rows: tuple[Row, ...]


def _require_mapping(value: Any, where: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise RenderError(f"{where} must be an object")
    return value


def _require_string(value: Any, where: str) -> str:
    if not isinstance(value, str) or not value.strip():
        raise RenderError(f"{where} must be a non-empty string")
    return value


def _require_number(value: Any, where: str) -> float:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise RenderError(f"{where} must be a number")
    number = float(value)
    if number != number or number in (float("inf"), float("-inf")):
        raise RenderError(f"{where} must be finite")
    return number


def _repo_root() -> pathlib.Path:
    return pathlib.Path(__file__).resolve().parent.parent


def _commit_date(root: pathlib.Path, commit: str) -> str:
    """Return the commit's committer date as YYYY-MM-DD, or fail loudly."""
    try:
        result = subprocess.run(
            ["git", "-C", str(root), "show", "-s", "--format=%cs", commit],
            check=False,
            capture_output=True,
            text=True,
        )
    except OSError as error:
        raise RenderError(f"cannot run git to date baseline commit {commit}: {error}") from error
    date = result.stdout.strip()
    if result.returncode != 0 or not date:
        detail = result.stderr.strip() or "no date returned"
        raise RenderError(
            f"cannot resolve baseline commit {commit} to a date: {detail}; "
            "fetch the repository history before rendering"
        )
    return date


def _duration(nanoseconds: float) -> str:
    """Format a nanosecond value with one stable unit and three decimals."""
    units = (
        ("s", 1_000_000_000.0),
        ("ms", 1_000_000.0),
        ("µs", 1_000.0),
        ("ns", 1.0),
    )
    for suffix, scale in units:
        if nanoseconds >= scale:
            return f"{nanoseconds / scale:.3f} {suffix}"
    return f"{nanoseconds:.3f} ns"


def _ratio(value: float) -> str:
    return f"{value:.3f}"


def _v1_entry(data: dict[str, Any], root: pathlib.Path) -> Entry:
    benches = _require_mapping(data.get("benches"), "benches")
    commit = _require_string(data.get("sha"), "sha")
    date = _commit_date(root, commit)
    rows: list[Row] = []
    for benchmark in sorted(benches):
        record = _require_mapping(benches[benchmark], f"benches[{benchmark!r}]")
        verdict = record.get("verdict")
        if verdict == "MEASURED":
            value = _duration(_require_number(record.get("median_ns"), f"benches[{benchmark!r}].median_ns"))
            aa = _ratio(_require_number(record.get("aa_spread"), f"benches[{benchmark!r}].aa_spread"))
        elif isinstance(verdict, str) and verdict.upper() == "UNMEASURED":
            value = UNMEASURED
            aa = UNMEASURED
        else:
            raise RenderError(
                f"benches[{benchmark!r}].verdict must be MEASURED or UNMEASURED"
            )
        rows.append(Row(benchmark, value, aa, commit, date))
    return Entry(commit, date, tuple(rows))


def _v2_entries(data: dict[str, Any], root: pathlib.Path) -> list[Entry]:
    targets = data.get("targets")
    if not isinstance(targets, (dict, list)):
        raise RenderError("targets must be an object or an array")

    if isinstance(targets, dict):
        target_items: Iterable[tuple[str, Any]] = targets.items()
    else:
        target_items = (
            (_require_string(target.get("name") if isinstance(target, dict) else None, "targets[].name"), target)
            for target in targets
        )

    entries: list[Entry] = []
    for target_name, target in target_items:
        target = _require_mapping(target, f"targets[{target_name!r}]")
        raw_entries = target.get("entries")
        if raw_entries is None:
            continue
        if not isinstance(raw_entries, list):
            raise RenderError(f"targets[{target_name!r}].entries must be an array")
        for index, raw_entry in enumerate(raw_entries):
            where = f"targets[{target_name!r}].entries[{index}]"
            entry = _require_mapping(raw_entry, where)
            commit = _require_string(entry.get("commit"), f"{where}.commit")
            date = _commit_date(root, commit)
            benchmarks = _require_mapping(entry.get("benchmarks"), f"{where}.benchmarks")
            rows: list[Row] = []
            for benchmark in sorted(benchmarks):
                where_benchmark = f"{where}.benchmarks[{benchmark!r}]"
                record = _require_mapping(benchmarks[benchmark], where_benchmark)
                rows.extend(
                    _v2_rows(
                        record,
                        f"{target_name}/{benchmark}",
                        where_benchmark,
                        commit,
                        date,
                    )
                )
            entries.append(Entry(commit, date, tuple(rows)))
    return entries


def _v2_rows(
    record: dict[str, Any],
    label: str,
    where: str,
    commit: str,
    date: str,
) -> list[Row]:
    """Turn one scenario record, keyed by metric, into table rows."""
    if not record:
        raise RenderError(f"{where} must name at least one metric")
    if "verdict" in record:
        raise RenderError(f"{where} must be keyed by metric, each metric holding its verdict")
    rows: list[Row] = []
    for metric in sorted(record):
        metric_record = _require_mapping(record[metric], f"{where}[{metric!r}]")
        rows.append(
            _v2_row(
                metric_record,
                f"{label}/{metric}",
                f"{where}[{metric!r}]",
                commit,
                date,
            )
        )
    return rows


def _as_emitted(value: Any, where: str) -> str:
    """A value that is not a timing, written as the benchmark emitted it."""
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise RenderError(f"{where} must be a number")
    if isinstance(value, float):
        _require_number(value, where)
        return repr(value)
    return str(value)


def _v2_row(
    record: dict[str, Any],
    label: str,
    where: str,
    commit: str,
    date: str,
) -> Row:
    """One metric record as the reference producer writes it.

    A record's ``kind`` is ``criterion``, ``bench-json`` or ``allocation``.  A
    record converted from a v1 file keeps its v1 fields as committed and has no
    ``kind``; v1 holds only Criterion medians, so it renders as one.  A
    bench-json statistic named ``*_ns`` is a timing; any other statistic and an
    allocation report are not timings and have no A/A ratio.
    """
    verdict = record.get("verdict")
    if isinstance(verdict, str) and verdict.upper() == UNMEASURED.upper():
        return Row(label, UNMEASURED, UNMEASURED, commit, date)
    if verdict != "MEASURED":
        raise RenderError(f"{where}.verdict must be MEASURED or UNMEASURED")

    kind = record.get("kind")
    if kind is None or kind == "criterion":
        value = _duration(_require_number(record.get("median_ns"), f"{where}.median_ns"))
        aa = _ratio(_require_number(record.get("aa_spread"), f"{where}.aa_spread"))
    elif kind == "bench-json":
        statistic = _require_string(record.get("statistic"), f"{where}.statistic")
        if statistic.endswith("_ns"):
            value = _duration(_require_number(record.get("value"), f"{where}.value"))
            aa = _ratio(_require_number(record.get("aa_spread"), f"{where}.aa_spread"))
        else:
            value = _as_emitted(record.get("value"), f"{where}.value")
            if "unit" in record:
                value = f"{value} {_require_string(record.get('unit'), f'{where}.unit')}"
            aa = NOT_A_TIMING
    elif kind == "allocation":
        report = _require_mapping(record.get("report"), f"{where}.report")
        fields = [
            f"{name}={_as_emitted(report[name], f'{where}.report[{name!r}]')}"
            for name in sorted(report)
            if name != "scenario"
        ]
        if not fields:
            raise RenderError(f"{where}.report has no counts")
        value = ", ".join(fields)
        aa = NOT_A_TIMING
    else:
        raise RenderError(f"{where}.kind {kind!r} is not criterion, bench-json or allocation")
    return Row(label, value, aa, commit, date)


def entries_for(data: dict[str, Any], root: pathlib.Path) -> tuple[str, list[Entry]]:
    schema = _require_string(data.get("schema"), "schema")
    crate = _require_string(data.get("crate"), "crate")
    if schema == "bench-reference/v1":
        return crate, [_v1_entry(data, root)]
    if schema == "bench-reference/v2":
        return crate, _v2_entries(data, root)
    raise RenderError(f"unsupported reference schema {schema!r}")


def _escape_cell(value: str) -> str:
    return value.replace("\\", "\\\\").replace("|", "\\|").replace("\n", " ")


def render_block(reference: pathlib.Path, root: pathlib.Path) -> str:
    try:
        raw = reference.read_text(encoding="utf-8")
    except OSError as error:
        raise RenderError(f"cannot read {reference}: {error}") from error
    try:
        data = json.loads(raw)
    except json.JSONDecodeError as error:
        raise RenderError(f"{reference} is not valid JSON: {error}") from error
    data = _require_mapping(data, str(reference))
    crate, entries = entries_for(data, root)
    if not entries:
        raise RenderError(f"{reference} has no measured target entries")

    headers = (
        "Benchmark",
        "Reference median",
        "A/A ratio",
        "Baseline commit",
        "Commit date",
    )
    cells = [
        [
            _escape_cell(f"`{row.benchmark}`"),
            _escape_cell(row.value),
            _escape_cell(row.aa_spread),
            _escape_cell(f"`{row.commit}`"),
            _escape_cell(row.date),
        ]
        for entry in entries
        for row in entry.rows
    ]
    widths = [
        max(len(headers[index]), *(len(row[index]) for row in cells))
        for index in range(len(headers))
    ]

    def table_row(values: tuple[str, ...] | list[str]) -> str:
        return "| " + " | ".join(
            value.ljust(widths[index]) for index, value in enumerate(values)
        ) + " |"

    lines = [
        BEGIN_MARKER,
        "",
        GENERATED_NOTICE,
        "",
        f"Reference baselines for `{crate}`. Values are the committed reference "
        "median for each benchmark, in the unit shown; A/A is the ratio between "
        "the two same-commit runs. The date is the committer date of the "
        "baseline commit.",
        "",
        table_row(headers),
        "| " + " | ".join("-" * width for width in widths) + " |",
    ]
    lines.extend(table_row(row) for row in cells)
    lines.extend(["", END_MARKER])
    return "\n".join(lines)


def replace_block(document: str, block: str, name: str) -> str:
    starts = document.count(BEGIN_MARKER)
    ends = document.count(END_MARKER)
    if starts == 0 and ends == 0:
        raise RenderError(f"{name} has no {BEGIN_MARKER} / {END_MARKER} markers")
    if starts != 1 or ends != 1:
        raise RenderError(
            f"{name} must contain exactly one {BEGIN_MARKER} and one {END_MARKER}"
        )
    begin = document.index(BEGIN_MARKER)
    end = document.index(END_MARKER)
    if end < begin:
        raise RenderError(f"{name} has {END_MARKER} before {BEGIN_MARKER}")
    return document[:begin] + block + document[end + len(END_MARKER):]


def render_document(reference: pathlib.Path, document: pathlib.Path, root: pathlib.Path) -> str:
    try:
        original = document.read_text(encoding="utf-8")
    except OSError as error:
        raise RenderError(f"cannot read {document}: {error}") from error
    return replace_block(original, render_block(reference, root), str(document))


def find_references(root: pathlib.Path) -> list[pathlib.Path]:
    return sorted((root / "crates").glob("*/perf/reference.json"))


def document_for(reference: pathlib.Path) -> pathlib.Path:
    return reference.parent.parent / "docs" / "benchmarks.md"


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--write", action="store_true", help="rewrite stale benchmark docs")
    mode.add_argument("--check", action="store_true", help="report stale benchmark docs")
    parser.add_argument(
        "--root",
        type=pathlib.Path,
        default=_repo_root(),
        help="repository root (default: the checkout containing this script)",
    )
    args = parser.parse_args(argv)
    root = args.root.resolve()

    references = find_references(root)
    if not references:
        print("no crates/*/perf/reference.json files found", file=sys.stderr)
        return 2

    failures: list[str] = []
    for reference in references:
        document = document_for(reference)
        label = document.relative_to(root)
        try:
            rendered = render_document(reference, document, root)
        except RenderError as error:
            failures.append(f"{label}: {error}")
            continue
        if rendered == document.read_text(encoding="utf-8"):
            print(f"up to date: {label}")
            continue
        if args.check:
            failures.append(f"{label}: stale; run scripts/render-bench-reference.py --write")
            continue
        document.write_text(rendered, encoding="utf-8")
        print(f"updated: {label}")

    if failures:
        for failure in failures:
            print(failure, file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
