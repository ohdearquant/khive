#!/usr/bin/env python3
"""Generate the AGENTS.md taxonomy from the default runtime's schema() response.

Use --kkernel PATH --write to regenerate, or --kkernel PATH --check in CI.
--schema-json accepts a captured schema result or its single-operation envelope.
It is an offline input, not a substitute for the live CI check.
"""

from __future__ import annotations

import argparse
import difflib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile


ROOT = Path(__file__).resolve().parents[1]
BEGIN = "<!-- BEGIN GENERATED TAXONOMY -->"
END = "<!-- END GENERATED TAXONOMY -->"
ARRAYS = ("entity_kinds", "note_kinds", "edge_relations", "endpoint_rules", "packs_loaded")
ENDPOINT_FIELDS = (
    "origin", "source_substrate", "source", "source_entity_type", "relation",
    "target_substrate", "target", "target_entity_type",
)
OPTIONAL_ENDPOINT_FIELDS = {"source_entity_type", "target_entity_type"}


def text_value(value, field):
    if not isinstance(value, str) or not value or any(ord(c) < 32 or ord(c) == 127 for c in value):
        raise ValueError(f"{field} must be a nonempty single-line string")
    return value


def schema_result(value, *, envelope_required=False):
    if isinstance(value, dict) and "results" in value:
        results = value["results"]
        summary = value.get("summary")
        if (not isinstance(results, list) or len(results) != 1
                or not isinstance(results[0], dict)
                or results[0].get("tool") != "schema" or results[0].get("ok") is not True
                or not isinstance(summary, dict)
                or any(type(summary.get(key)) is not int or summary[key] != expected
                       for key, expected in (("total", 1), ("succeeded", 1), ("failed", 0)))):
            raise ValueError("expected exactly one successful schema operation")
        value = results[0].get("result")
    elif envelope_required:
        raise ValueError("kkernel did not return a schema request envelope")
    if not isinstance(value, dict) or set(value) != set(ARRAYS) | {"counts", "contract_version"}:
        raise ValueError("schema has missing or unrecognized fields")
    counts = value["counts"]
    if not isinstance(counts, dict) or set(counts) != set(ARRAYS):
        raise ValueError("schema counts have missing or unrecognized fields")
    for field in ARRAYS:
        items = value[field]
        if not isinstance(items, list) or type(counts[field]) is not int or counts[field] != len(items):
            raise ValueError(f"schema count does not match {field}")
        if field != "endpoint_rules":
            for item in items:
                text_value(item, field)
            if len(set(items)) != len(items):
                raise ValueError(f"schema {field} contains duplicate values")
    if "kg" not in value["packs_loaded"]:
        raise ValueError("schema does not include its owning kg pack")
    if not isinstance(value["contract_version"], str) or not re.fullmatch(
            r"[0-9a-f]{64}", value["contract_version"]):
        raise ValueError("schema contract_version must be a full lowercase content hash")
    required = set(ENDPOINT_FIELDS) - OPTIONAL_ENDPOINT_FIELDS
    for row in value["endpoint_rules"]:
        if not isinstance(row, dict) or not required <= set(row) <= set(ENDPOINT_FIELDS):
            raise ValueError("endpoint rule has missing or unrecognized fields")
        for field, item in row.items():
            text_value(item, field)
        if row["origin"] != "base" and row["origin"] not in {
                f"pack:{pack}" for pack in value["packs_loaded"]}:
            raise ValueError("endpoint rule names an unloaded origin")
        if row["relation"] not in value["edge_relations"]:
            raise ValueError("endpoint rule names an undeclared relation")
        for side in ("source", "target"):
            if row[f"{side}_substrate"] not in ("entity", "note"):
                raise ValueError("endpoint rule has an unknown substrate")
            if f"{side}_entity_type" in row and row[f"{side}_substrate"] != "entity":
                raise ValueError("endpoint entity type requires the entity substrate")
    return value


def acquire_schema(binary, timeout):
    binary = str(Path(binary).resolve(strict=True))
    env = {key: value for key, value in os.environ.items() if not key.startswith("KHIVE_")}
    env["KHIVE_NO_DAEMON"] = "1"
    # An explicit empty config avoids cwd/HOME discovery without discarding the model cache.
    with tempfile.TemporaryDirectory(prefix="khive-taxonomy-") as directory:
        config = Path(directory) / "empty.toml"
        config.write_text("", encoding="utf-8")
        config.chmod(0o600)
        result = subprocess.run(
            [binary, "exec", "schema()", "--db", ":memory:", "--config", str(config),
             "--namespace", "local", "--actor", "local", "--expect-actor", "local",
             "--presentation", "verbose", "--output-format", "json", "--strict"],
            cwd=directory, env=env, text=True, encoding="utf-8", capture_output=True,
            timeout=timeout, check=False,
        )
    if result.returncode:
        raise ValueError(f"kkernel schema() exited with status {result.returncode}")
    return schema_result(json.loads(result.stdout), envelope_required=True)


def code(value):
    value = value.replace("|", "\\|")
    fence = "`" * (1 + max((len(run) for run in re.findall(r"`+", value)), default=0))
    if value.startswith(("`", " ")) or value.endswith(("`", " ")):
        value = " " + value + " "
    return fence + value + fence


def table(headers, rows):
    widths = [max(3, len(header), *(len(row[index]) for row in rows))
              for index, header in enumerate(headers)]

    def line(cells):
        return "| " + " | ".join(cell.ljust(width) for cell, width in zip(cells, widths)) + " |"

    return "\n".join([line(headers), line(["-" * width for width in widths]),
                      *(line(row) for row in rows)])


def render_schema(schema):
    schema = schema_result(schema)
    lines = [
        "## Loaded taxonomy",
        "",
        "Generated from `schema()` with the binary's default pack selection. Custom pack selections",
        "can change the vocabulary and endpoint rules; inspect `schema()` on that server.",
        "",
        "Regenerate with `python3 scripts/generate-taxonomy.py --kkernel <path> --write`.",
        "",
        f"Contract version: {code(schema['contract_version'])}.",
        "",
        "Loaded packs: " + ", ".join(code(pack) for pack in sorted(schema["packs_loaded"])) + ".",
    ]
    for field, title in (("entity_kinds", "Entity kinds"), ("note_kinds", "Note kinds"),
                         ("edge_relations", "Edge relations")):
        lines += ["", f"### {title} ({schema['counts'][field]})", ""]
        lines += [f"- {code(item)}" for item in sorted(schema[field])]
    rows = [[code(row[field]) if field in row else "—" for field in ENDPOINT_FIELDS]
            for row in sorted(schema["endpoint_rules"],
                              key=lambda row: tuple(row.get(field, "") for field in ENDPOINT_FIELDS))]
    lines += [
        "", f"### Endpoint rules ({schema['counts']['endpoint_rules']})", "",
        "`base` names a base rule; `pack:NAME` names the pack that declares an additional rule.",
        "`*` is a literal wildcard. An absent entity type (—) imposes no subtype restriction.",
        "Duplicate declarations are retained.", "",
        table(["Origin", "Source substrate", "Source kind", "Source entity type", "Relation",
               "Target substrate", "Target kind", "Target entity type"], rows),
    ]
    return "\n".join(lines)


def replace_region(document, rendered):
    if document.count(BEGIN) != 1 or document.count(END) != 1:
        raise ValueError("document must contain exactly one generated taxonomy marker pair")
    begin = re.search(r"^" + re.escape(BEGIN) + r"\r?$", document, re.MULTILINE)
    end = re.search(r"^" + re.escape(END) + r"\r?$", document, re.MULTILINE)
    if begin is None or end is None or begin.end() >= end.start():
        raise ValueError("taxonomy markers must be ordered on separate lines")
    return document[:begin.end()] + "\n\n" + rendered + "\n\n" + document[end.start():]


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument("--kkernel", type=Path, help="binary built from the documentation's revision")
    source.add_argument("--schema-json", type=Path, help="captured schema result or request envelope")
    parser.add_argument("--document", type=Path, default=ROOT / "AGENTS.md")
    parser.add_argument("--timeout", type=float, default=120, help="schema process deadline in seconds")
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--write", action="store_true")
    mode.add_argument("--check", action="store_true", help="check without writing (the default)")
    args = parser.parse_args(argv)
    if not 0 < args.timeout <= 600:
        parser.error("--timeout must be greater than zero and at most 600 seconds")
    try:
        original = args.document.read_bytes().decode("utf-8")
        replace_region(original, "")  # Refuse an invalid document before invoking the binary.
        schema = (acquire_schema(args.kkernel, args.timeout) if args.kkernel else
                  schema_result(json.loads(args.schema_json.read_text(encoding="utf-8"))))
        rendered = render_schema(schema)
        generated = replace_region(original, rendered)
        if generated == original:
            print("Taxonomy matches schema().")
            return 0
        if args.write:
            args.document.write_bytes(generated.encode("utf-8"))
            print(f"Updated {args.document} from schema().")
            return 0
        sys.stderr.writelines(difflib.unified_diff(
            original.splitlines(keepends=True), generated.splitlines(keepends=True),
            fromfile=str(args.document), tofile="generated from schema()",
        ))
        print("Taxonomy differs; regenerate with --write using the same revision's binary.",
              file=sys.stderr)
        print("Full replacement block from schema():")
        print(BEGIN + "\n\n" + rendered + "\n\n" + END)
        return 1
    except (OSError, ValueError, subprocess.TimeoutExpired) as error:
        print(f"Cannot generate taxonomy: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
