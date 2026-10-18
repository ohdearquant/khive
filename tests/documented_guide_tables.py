#!/usr/bin/env python3
"""Validate the tables in the developer guide that restate facts the tree holds.

The guide (``CLAUDE.md``) keeps two kinds of table that can be derived from
the repository, so drift in either is a failing check rather than a reading:

* The directory map has one row per crate: every ``crates/<name>/Cargo.toml``
  in the tree, and no ``crates/`` row naming a directory without one. Its other
  rows name paths that must exist.
* Each ``### <Name> pack verbs (...)`` table lists one pack's verbs. Its verb
  column must equal the verbs ``verbs()`` reports for that pack. Its Args
  column lists the commonly used parameters, not every one (``verbs()`` returns
  the full signature), so it is held to two rules: every parameter it names
  exists on that verb, and a parameter the registry requires is not written
  with ``?``. Backticked text inside parentheses is an annotation, such as an
  allowed value or an alias, and is not read as a parameter.

``check_directory_map`` needs only the tree. ``validate_guide_verb_tables``
needs a ``verbs()`` result and runs inside the binary smoke test. To regenerate
rows, run ``python3 tests/documented_guide_tables.py --missing-rows <verbs.json>``
with a saved ``verbs()`` result: it prints a row for each crate the directory
map lacks (purpose taken from the crate's ``description``) and for each verb a
pack table lacks (every parameter from the registry), for the author to place
and describe.
"""

from __future__ import annotations

from dataclasses import dataclass
import json
from pathlib import Path
import re
import sys
import tomllib


GUIDE = "CLAUDE.md"
DIRECTORY_MAP_HEADING = "## Directory map"
VERB_TABLE_HEADING_RE = re.compile(r"^### (?P<name>[A-Za-z]+) pack verbs \(")
ROW_RE = re.compile(r"^\|")
SEPARATOR_RE = re.compile(r"^\|\s*:?-{3,}")
CODE_RE = re.compile(r"`([^`]*)`")
PARAM_RE = re.compile(r"^(?P<name>[a-z_][a-z0-9_]*)(?P<optional>\?)?(?:=.*)?$")
SIGNATURE_RE = re.compile(r"^(?P<verb>[a-z_][a-z0-9_.]*)\((?P<params>.*)\)$")


@dataclass(frozen=True)
class Row:
    line: int
    cells: tuple[str, ...]


@dataclass(frozen=True)
class VerbTable:
    line: int
    pack: str
    rows: tuple[Row, ...]


def split_row(line: str) -> tuple[str, ...]:
    """Split one Markdown table row on its unescaped pipes; ``\\|`` stays in its cell."""
    body = line.strip()
    if not (body.startswith("|") and body.endswith("|")) or len(body) < 2:
        raise ValueError(f"not a table row: {line!r}")
    cells: list[str] = []
    current: list[str] = []
    index = 1
    while index < len(body) - 1:
        char = body[index]
        if char == "\\" and body[index + 1] == "|":
            current.append("|")
            index += 2
            continue
        if char == "|":
            cells.append("".join(current).strip())
            current = []
        else:
            current.append(char)
        index += 1
    cells.append("".join(current).strip())
    return tuple(cells)


def _section(lines: list[str], start: int) -> list[tuple[int, str]]:
    """The lines after ``start`` up to the next heading of any level, numbered from 1."""
    section = []
    for index in range(start + 1, len(lines)):
        if lines[index].startswith("#"):
            break
        section.append((index + 1, lines[index]))
    return section


def _table_rows(section: list[tuple[int, str]]) -> tuple[Row, ...]:
    """The body rows of the first table in a section: header and separator are skipped."""
    rows: list[Row] = []
    seen_separator = False
    for number, line in section:
        if not ROW_RE.match(line):
            if seen_separator:
                break
            continue
        if SEPARATOR_RE.match(line):
            seen_separator = True
            continue
        if seen_separator:
            rows.append(Row(number, split_row(line)))
    return tuple(rows)


def parse_verb_tables(text: str) -> list[VerbTable]:
    lines = text.split("\n")
    tables = []
    for index, line in enumerate(lines):
        match = VERB_TABLE_HEADING_RE.match(line)
        if match:
            rows = _table_rows(_section(lines, index))
            tables.append(VerbTable(index + 1, match.group("name").lower(), rows))
    return tables


def directory_map_rows(text: str) -> tuple[Row, ...]:
    lines = text.split("\n")
    try:
        start = lines.index(DIRECTORY_MAP_HEADING)
    except ValueError:
        raise ValueError(f"{GUIDE} has no {DIRECTORY_MAP_HEADING!r} section") from None
    rows = _table_rows(_section(lines, start))
    if not rows:
        raise ValueError(f"{GUIDE}:{start + 1}: the directory map has no rows")
    return rows


def _only_code(cell: str) -> str | None:
    """The text of a cell that is exactly one backticked span, else None."""
    match = re.fullmatch(r"`([^`]+)`", cell.strip())
    return match.group(1) if match else None


def documented_params(cell: str) -> list[tuple[str, bool]]:
    """Parameters an Args cell names, as (name, written optional).

    A backticked identifier, optionally followed by ``?`` and by ``=...``, is a
    parameter unless it sits inside parentheses, where it annotates the
    parameter before it."""
    params = []
    depth = 0
    position = 0
    for match in CODE_RE.finditer(cell):
        outside = cell[position : match.start()]
        depth += outside.count("(") - outside.count(")")
        position = match.end()
        if depth > 0:
            continue
        param = PARAM_RE.match(match.group(1))
        if param:
            params.append((param.group("name"), param.group("optional") is not None))
    return params


def registry_verbs(verbs_result: object) -> dict[str, dict[str, str]]:
    """``{pack: {verb: signature}}`` from a ``verbs()`` result."""
    if not isinstance(verbs_result, dict) or not isinstance(verbs_result.get("verbs"), list):
        raise ValueError("verbs() result must be an object with a 'verbs' list")
    packs: dict[str, dict[str, str]] = {}
    for entry in verbs_result["verbs"]:
        if not isinstance(entry, dict):
            raise ValueError(f"verbs() entry is not an object: {entry!r}")
        verb, pack, signature = entry.get("verb"), entry.get("pack"), entry.get("signature")
        if not all(isinstance(value, str) and value for value in (verb, pack, signature)):
            raise ValueError(f"verbs() entry lacks verb, pack or signature: {entry!r}")
        packs.setdefault(pack, {})[verb] = signature
    return packs


def signature_params(verb: str, signature: str) -> dict[str, bool]:
    """``{param: required}`` from a ``verbs()`` signature such as ``name(a, b?)``."""
    match = SIGNATURE_RE.match(signature)
    if not match or match.group("verb") != verb:
        raise ValueError(f"unparsable signature for {verb}: {signature!r}")
    params: dict[str, bool] = {}
    for raw in match.group("params").split(","):
        raw = raw.strip()
        if not raw:
            continue
        param = PARAM_RE.match(raw)
        if not param or param.group(0) != raw or "=" in raw:
            raise ValueError(f"unparsable parameter {raw!r} in {signature!r}")
        params[param.group("name")] = param.group("optional") is None
    return params


def validate_guide_verb_tables(repo_root: Path, verbs_result: object) -> list[str]:
    registry = registry_verbs(verbs_result)
    text = (repo_root / GUIDE).read_text(encoding="utf-8")
    tables = parse_verb_tables(text)
    errors: list[str] = []
    if not tables:
        return [f"{GUIDE} has no '### <Name> pack verbs' table"]
    for table in tables:
        where = f"{GUIDE}:{table.line}"
        if table.pack not in registry:
            errors.append(f"{where}: no loaded pack {table.pack!r} reports verbs")
            continue
        if not table.rows:
            errors.append(f"{where}: the {table.pack} verb table has no rows")
            continue
        pack_verbs = registry[table.pack]
        documented: dict[str, Row] = {}
        for row in table.rows:
            verb = _only_code(row.cells[0])
            if verb is None:
                errors.append(f"{GUIDE}:{row.line}: verb cell is not one code span: {row.cells[0]!r}")
                continue
            if verb in documented:
                errors.append(f"{GUIDE}:{row.line}: {verb} is listed twice")
                continue
            documented[verb] = row
        for verb in sorted(set(pack_verbs) - set(documented)):
            errors.append(f"{where}: the {table.pack} table has no row for {verb}")
        for verb in sorted(set(documented) - set(pack_verbs)):
            errors.append(
                f"{GUIDE}:{documented[verb].line}: {verb} is not a {table.pack} verb in the registry"
            )
        for verb, row in documented.items():
            if verb not in pack_verbs or len(row.cells) < 2:
                continue
            try:
                signature = signature_params(verb, pack_verbs[verb])
            except ValueError as exc:
                errors.append(f"{GUIDE}:{row.line}: {exc}")
                continue
            for name, written_optional in documented_params(row.cells[1]):
                if name not in signature:
                    errors.append(f"{GUIDE}:{row.line}: {verb} has no parameter {name!r}")
                elif written_optional and signature[name]:
                    errors.append(
                        f"{GUIDE}:{row.line}: {verb} requires {name!r} but the guide writes {name}?"
                    )
    return errors


def crate_directories(repo_root: Path) -> list[str]:
    return sorted(path.parent.name for path in (repo_root / "crates").glob("*/Cargo.toml"))


def check_directory_map(repo_root: Path) -> list[str]:
    text = (repo_root / GUIDE).read_text(encoding="utf-8")
    rows = directory_map_rows(text)
    crates = crate_directories(repo_root)
    if not crates:
        return ["no crates/*/Cargo.toml found under the repository root"]
    errors: list[str] = []
    listed: dict[str, int] = {}
    for row in rows:
        path = _only_code(row.cells[0])
        if path is None:
            errors.append(f"{GUIDE}:{row.line}: path cell is not one code span: {row.cells[0]!r}")
            continue
        if path.startswith("crates/"):
            name = path.removeprefix("crates/").rstrip("/")
            if name in listed:
                errors.append(f"{GUIDE}:{row.line}: crates/{name} is listed twice")
            listed[name] = row.line
            if not (repo_root / "crates" / name / "Cargo.toml").is_file():
                errors.append(f"{GUIDE}:{row.line}: {path} has no Cargo.toml")
        elif not (repo_root / path).exists():
            errors.append(f"{GUIDE}:{row.line}: {path} does not exist")
    for name in crates:
        if name not in listed:
            errors.append(f"{GUIDE}: the directory map has no row for crates/{name}")
    return errors


def missing_rows(repo_root: Path, verbs_result: object) -> list[str]:
    text = (repo_root / GUIDE).read_text(encoding="utf-8")
    out: list[str] = []
    listed = {
        (_only_code(row.cells[0]) or "").removeprefix("crates/").rstrip("/")
        for row in directory_map_rows(text)
    }
    for name in crate_directories(repo_root):
        if name in listed:
            continue
        manifest = tomllib.loads((repo_root / "crates" / name / "Cargo.toml").read_text())
        description = manifest.get("package", {}).get("description", "")
        out.append(f"| `crates/{name}` | {description} |")
    registry = registry_verbs(verbs_result)
    for table in parse_verb_tables(text):
        documented = {_only_code(row.cells[0]) for row in table.rows}
        for verb, signature in sorted(registry.get(table.pack, {}).items()):
            if verb in documented:
                continue
            params = signature_params(verb, signature)
            args = ", ".join(f"`{name}{'' if required else '?'}`" for name, required in params.items())
            out.append(f"| `{verb}` | {args or '—'} | |")
    return out


if __name__ == "__main__":
    if len(sys.argv) != 3 or sys.argv[1] != "--missing-rows":
        print("usage: documented_guide_tables.py --missing-rows <verbs.json>", file=sys.stderr)
        sys.exit(2)
    root = Path(__file__).resolve().parent.parent
    result = json.loads(Path(sys.argv[2]).read_text())
    print("\n".join(missing_rows(root, result)))
