#!/usr/bin/env python3
"""Render a stable, fail-closed writer-path census from pack introspection."""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess
import sys
from typing import Any


SCHEMA = "khive.writer-census.manifest.v1"
REPORT_SCHEMA = "khive.writer-census.report.v1"
CLASSIFICATIONS = frozenset(
    {"WRITER", "WRITER-COND", "NO-WRITER", "UNKNOWN"}
)
DEFAULT_MANIFEST = Path(__file__).with_name("data") / "writer-census-v1.json"
DEFAULT_REPO_ROOT = Path(__file__).resolve().parents[1]


class CensusError(ValueError):
    """The manifest or observed inventory cannot produce a sound census."""


def canonical_json(value: object) -> str:
    return json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n"


def _classification(value: Any, context: str) -> str:
    if not isinstance(value, str) or value not in CLASSIFICATIONS:
        raise CensusError(
            f"{context}: invalid classification {value!r}; expected one of "
            f"{sorted(CLASSIFICATIONS)}"
        )
    return value


def _string_list(value: Any, context: str) -> list[str]:
    if not isinstance(value, list) or any(
        not isinstance(item, str) or not item for item in value
    ):
        raise CensusError(f"{context}: expected a list of non-empty strings")
    if len(value) != len(set(value)):
        raise CensusError(f"{context}: duplicate values are not allowed")
    return value


def _validate_path(path: Any, context: str) -> dict[str, Any]:
    if not isinstance(path, dict):
        raise CensusError(f"{context}: writer path must be an object")
    normalized = dict(path)
    normalized["classification"] = _classification(
        normalized.get("classification"), f"{context}.classification"
    )
    for field in ("kind", "symbol"):
        if not isinstance(normalized.get(field), str) or not normalized[field]:
            raise CensusError(f"{context}.{field}: expected a non-empty string")
    if normalized["classification"] == "WRITER-COND" and (
        not isinstance(normalized.get("condition"), str)
        or not normalized["condition"].strip()
    ):
        raise CensusError(
            f"{context}.condition: WRITER-COND paths must name their condition"
        )
    if normalized["classification"] != "UNKNOWN":
        evidence = normalized.get("evidence")
        if not isinstance(evidence, dict):
            raise CensusError(
                f"{context}.evidence: known writer paths require pinned source evidence"
            )
        evidence_path = evidence.get("path")
        if (
            not isinstance(evidence_path, str)
            or not evidence_path
            or Path(evidence_path).is_absolute()
            or ".." in Path(evidence_path).parts
        ):
            raise CensusError(
                f"{context}.evidence.path: expected a repository-relative path"
            )
        _string_list(
            evidence.get("required_patterns"),
            f"{context}.evidence.required_patterns",
        )
    return normalized


def _validate_entry(entry: Any, context: str) -> dict[str, Any]:
    if not isinstance(entry, dict):
        raise CensusError(f"{context}: expected an object")
    normalized = dict(entry)
    normalized["classification"] = _classification(
        normalized.get("classification"), f"{context}.classification"
    )
    if not isinstance(normalized.get("reason"), str) or not normalized["reason"]:
        raise CensusError(f"{context}.reason: expected a non-empty string")
    paths = normalized.get("paths", [])
    if not isinstance(paths, list):
        raise CensusError(f"{context}.paths: expected a list")
    normalized["paths"] = [
        _validate_path(path, f"{context}.paths[{index}]")
        for index, path in enumerate(paths)
    ]
    nested = normalized.get("nested_dispatches", [])
    if not isinstance(nested, list):
        raise CensusError(f"{context}.nested_dispatches: expected a list")
    for index, dispatch in enumerate(nested):
        if not isinstance(dispatch, dict):
            raise CensusError(
                f"{context}.nested_dispatches[{index}]: expected an object"
            )
        for field in ("target", "condition"):
            if not isinstance(dispatch.get(field), str) or not dispatch[field]:
                raise CensusError(
                    f"{context}.nested_dispatches[{index}].{field}: "
                    "expected a non-empty string"
                )
        if dispatch.get("via_registry") is not True:
            raise CensusError(
                f"{context}.nested_dispatches[{index}].via_registry: "
                "nested dispatches in v1 must explicitly traverse the registry"
            )
    if normalized["classification"] == "WRITER" and not any(
        path["classification"] == "WRITER" for path in normalized["paths"]
    ):
        raise CensusError(
            f"{context}: WRITER classification requires a WRITER path"
        )
    if (
        normalized["classification"] == "NO-WRITER"
        and normalized.get("trace_complete") is not True
    ):
        raise CensusError(
            f"{context}: NO-WRITER requires trace_complete=true; absence of "
            "evidence is UNKNOWN"
        )
    return normalized


def _validate_manifest(manifest: Any) -> dict[str, Any]:
    if not isinstance(manifest, dict):
        raise CensusError("manifest: expected an object")
    if manifest.get("schema_version") != SCHEMA:
        raise CensusError(
            f"manifest.schema_version: expected {SCHEMA!r}, got "
            f"{manifest.get('schema_version')!r}"
        )
    revision = manifest.get("source_revision")
    if not isinstance(revision, str) or re.fullmatch(r"[0-9a-f]{40}", revision) is None:
        raise CensusError(
            "manifest.source_revision: expected a lowercase 40-hex commit"
        )
    pack_set = _string_list(manifest.get("pack_set"), "manifest.pack_set")
    inventory = manifest.get("inventory")
    if not isinstance(inventory, dict):
        raise CensusError("manifest.inventory: expected an object")
    if set(inventory) != set(pack_set):
        raise CensusError(
            "manifest.inventory keys must exactly match manifest.pack_set"
        )
    public_owner: dict[str, str] = {}
    for pack in pack_set:
        for verb in _string_list(
            inventory[pack], f"manifest.inventory.{pack}"
        ):
            if verb in public_owner:
                raise CensusError(
                    f"manifest.inventory: verb {verb!r} appears in both "
                    f"{public_owner[verb]!r} and {pack!r}"
                )
            public_owner[verb] = pack

    defaults = _validate_entry(manifest.get("defaults"), "manifest.defaults")
    if defaults["classification"] != "UNKNOWN":
        raise CensusError(
            "manifest.defaults.classification must be UNKNOWN so untraced "
            "handlers fail closed"
        )
    internal_raw = manifest.get("internal_handlers", {})
    overrides_raw = manifest.get("overrides", {})
    if not isinstance(internal_raw, dict) or not isinstance(overrides_raw, dict):
        raise CensusError("manifest handlers and overrides must be objects")
    internal = {
        name: _validate_entry(entry, f"manifest.internal_handlers.{name}")
        for name, entry in internal_raw.items()
    }
    overrides = {
        name: _validate_entry(entry, f"manifest.overrides.{name}")
        for name, entry in overrides_raw.items()
    }
    unknown_overrides = sorted(set(overrides) - set(public_owner))
    if unknown_overrides:
        raise CensusError(
            "manifest.overrides names verbs outside the pinned inventory: "
            + ", ".join(unknown_overrides)
        )

    control = manifest.get("control")
    if not isinstance(control, dict):
        raise CensusError("manifest.control: expected an object")
    control_verb = control.get("verb")
    if not isinstance(control_verb, str) or not control_verb:
        raise CensusError("manifest.control.verb: expected a non-empty string")
    required = _classification(
        control.get("required_classification"),
        "manifest.control.required_classification",
    )

    normalized = dict(manifest)
    normalized["pack_set"] = pack_set
    normalized["public_owner"] = public_owner
    normalized["defaults"] = defaults
    normalized["internal_handlers"] = internal
    normalized["overrides"] = overrides
    normalized["control"] = {
        "verb": control_verb,
        "required_classification": required,
    }
    return normalized


def _observed_inventory(
    raw: Any, pack_set: list[str]
) -> tuple[dict[str, str], list[str]]:
    if not isinstance(raw, list):
        raise CensusError("observed inventory: expected `kkernel pack list` JSON")
    selected = set(pack_set)
    seen_packs: set[str] = set()
    owner: dict[str, str] = {}
    for pack in raw:
        if not isinstance(pack, dict) or not isinstance(pack.get("name"), str):
            raise CensusError("observed inventory: each pack must name itself")
        pack_name = pack["name"]
        if pack_name not in selected:
            continue
        if pack_name in seen_packs:
            raise CensusError(f"observed inventory: duplicate pack {pack_name!r}")
        seen_packs.add(pack_name)
        verbs = pack.get("verbs")
        if not isinstance(verbs, list):
            raise CensusError(
                f"observed inventory pack {pack_name!r}: verbs must be a list"
            )
        for raw_verb in verbs:
            if isinstance(raw_verb, str):
                verb = raw_verb
                visibility = "verb"
            elif isinstance(raw_verb, dict):
                verb = raw_verb.get("name")
                visibility = raw_verb.get("visibility", "verb")
            else:
                raise CensusError(
                    f"observed inventory pack {pack_name!r}: malformed verb"
                )
            if visibility != "verb":
                continue
            if not isinstance(verb, str) or not verb:
                raise CensusError(
                    f"observed inventory pack {pack_name!r}: verb needs a name"
                )
            if verb in owner:
                raise CensusError(
                    f"observed inventory: public verb {verb!r} has duplicate owners"
                )
            owner[verb] = pack_name
    missing_packs = sorted(selected - seen_packs)
    return owner, missing_packs


def _sorted_paths(paths: list[dict[str, Any]]) -> list[dict[str, Any]]:
    return sorted(paths, key=canonical_json)


def _revision_is_commit(repo_root: Path, revision: str) -> bool:
    completed = subprocess.run(
        ["git", "-C", str(repo_root), "cat-file", "-t", revision],
        check=False,
        capture_output=True,
        text=True,
    )
    return completed.returncode == 0 and completed.stdout.strip() == "commit"


def _verify_path_evidence(
    path: dict[str, Any], repo_root: Path, revision: str
) -> tuple[dict[str, Any], str | None]:
    if path["classification"] == "UNKNOWN":
        return path, None
    evidence = path["evidence"]
    completed = subprocess.run(
        [
            "git",
            "-C",
            str(repo_root),
            "show",
            f"{revision}:{evidence['path']}",
        ],
        check=False,
        capture_output=True,
        text=True,
    )
    if completed.returncode != 0:
        failure = (
            f"cannot read {evidence['path']!r} at pinned revision {revision}"
        )
    else:
        missing = [
            pattern
            for pattern in evidence["required_patterns"]
            if pattern not in completed.stdout
        ]
        failure = (
            f"pinned evidence {evidence['path']!r} is missing patterns {missing!r}"
            if missing
            else None
        )
    if failure is None:
        return path, None
    failed = dict(path)
    failed["classification"] = "UNKNOWN"
    failed["evidence_error"] = failure
    return failed, failure


def _verified_paths(
    paths: list[dict[str, Any]],
    repo_root: Path,
    revision: str,
    owner: str,
) -> tuple[list[dict[str, Any]], list[str]]:
    verified: list[dict[str, Any]] = []
    warnings: list[str] = []
    for path in paths:
        checked, failure = _verify_path_evidence(path, repo_root, revision)
        verified.append(checked)
        if failure is not None:
            warnings.append(f"{owner}: {failure}")
    return _sorted_paths(verified), warnings


def _effective_classification(
    declared: str, paths: list[dict[str, Any]]
) -> str:
    if declared == "WRITER" and not any(
        path["classification"] == "WRITER" for path in paths
    ):
        return "UNKNOWN"
    if declared == "WRITER-COND" and not any(
        path["classification"] == "WRITER-COND" for path in paths
    ):
        return "UNKNOWN"
    if declared == "NO-WRITER" and any(
        path["classification"] in {"WRITER", "WRITER-COND"}
        for path in paths
    ):
        return "UNKNOWN"
    if declared == "NO-WRITER" and (
        not paths
        or all(path["classification"] == "UNKNOWN" for path in paths)
    ):
        return "UNKNOWN"
    return declared


def _public_entry(
    manifest: dict[str, Any],
    verb: str,
    pack: str,
    pinned: bool,
    repo_root: Path,
    evidence_revision: str,
) -> tuple[dict[str, Any], list[str]]:
    warnings: list[str] = []
    if not pinned:
        return (
            {
                "classification": "UNKNOWN",
                "nested_dispatches": [],
                "pack": pack,
                "reason": "manifest entry missing",
                "verb": verb,
                "writer_paths": [],
            },
            [f"observed verb {verb!r} is absent from the pinned manifest"],
        )

    defaults = manifest["defaults"]
    override = manifest["overrides"].get(verb)
    selected = override if override is not None else defaults
    paths = list(defaults["paths"])
    if override is not None:
        if override.get("inherit_default_paths", True):
            paths.extend(override["paths"])
        else:
            paths = list(override["paths"])
    paths, evidence_warnings = _verified_paths(
        paths, repo_root, evidence_revision, verb
    )
    warnings.extend(evidence_warnings)

    nested_rows: list[dict[str, Any]] = []
    for dispatch in selected.get("nested_dispatches", []):
        target = dispatch["target"]
        target_entry = manifest["internal_handlers"].get(target)
        target_paths = list(manifest["defaults"]["paths"])
        if target_entry is None:
            resolved = "UNKNOWN"
            warnings.append(
                f"nested dispatch target {target!r} from {verb!r} is unclassified"
            )
        else:
            target_paths.extend(target_entry["paths"])
            target_paths, target_warnings = _verified_paths(
                target_paths,
                repo_root,
                evidence_revision,
                f"{verb} -> {target}",
            )
            warnings.extend(target_warnings)
            resolved = _effective_classification(
                target_entry["classification"], target_paths
            )
        nested_rows.append(
            {
                "condition": dispatch["condition"],
                "resolved_classification": resolved,
                "target": target,
                "via_registry": True,
                "writer_paths": target_paths,
            }
        )

    all_writer_paths = paths + [
        path
        for nested in nested_rows
        for path in nested["writer_paths"]
    ]
    declared = selected["classification"]
    effective = _effective_classification(declared, all_writer_paths)
    reason = selected["reason"]
    if effective != declared:
        if declared == "NO-WRITER" and any(
            path["classification"] in {"WRITER", "WRITER-COND"}
            for path in all_writer_paths
        ):
            reason = "declared NO-WRITER is contradicted by verified writer evidence"
        elif declared == "NO-WRITER":
            reason = "declared NO-WRITER carries no verified read-only evidence"
        else:
            reason = f"declared {declared} lacks verified matching writer evidence"
        warnings.append(f"{verb}: {reason}")

    return (
        {
            "classification": effective,
            "nested_dispatches": sorted(
                nested_rows, key=lambda row: (row["target"], row["condition"])
            ),
            "pack": pack,
            "reason": reason,
            "verb": verb,
            "writer_paths": _sorted_paths(paths),
        },
        warnings,
    )


def _void_report(
    manifest: dict[str, Any],
    errors: list[str],
    warnings: list[str],
    observed_revision: str | None,
) -> dict[str, Any]:
    return {
        "errors": sorted(set(errors)),
        "observed_revision": observed_revision,
        "pack_set": sorted(manifest["pack_set"]),
        "schema_version": REPORT_SCHEMA,
        "source_revision": manifest["source_revision"],
        "status": "VOID",
        "warnings": sorted(set(warnings)),
    }


def build_report(
    manifest: Any,
    observed_inventory: Any,
    *,
    repo_root: Path = DEFAULT_REPO_ROOT,
    observed_revision: str | None = None,
) -> dict[str, Any]:
    checked = _validate_manifest(manifest)
    observed, missing_packs = _observed_inventory(
        observed_inventory, checked["pack_set"]
    )
    errors = [
        f"configured pack {pack!r} is absent from the observed inventory"
        for pack in missing_packs
    ]
    warnings: list[str] = []
    evidence_revision = checked["source_revision"]
    if (
        not isinstance(observed_revision, str)
        or re.fullmatch(r"[0-9a-f]{40}", observed_revision) is None
    ):
        errors.append(
            "observed artifact revision is absent or invalid; census is void"
        )
    elif not _revision_is_commit(repo_root, observed_revision):
        errors.append(
            "observed artifact revision does not resolve to a commit; "
            "census is void"
        )
    elif observed_revision != checked["source_revision"]:
        evidence_revision = observed_revision
        warnings.append(
            f"observed artifact revision {observed_revision} differs from "
            f"manifest source revision {checked['source_revision']}; every "
            "evidence pattern was re-verified at the observed revision"
        )
    pinned_owner = checked["public_owner"]
    entries: list[dict[str, Any]] = []
    for verb, pack in sorted(observed.items()):
        entry, entry_warnings = _public_entry(
            checked, verb, pack, verb in pinned_owner, repo_root,
            evidence_revision,
        )
        entries.append(entry)
        warnings.extend(entry_warnings)

    for verb in sorted(set(pinned_owner) - set(observed)):
        warnings.append(
            f"pinned verb {verb!r} is absent from the observed inventory"
        )

    control = checked["control"]
    control_entry = next(
        (entry for entry in entries if entry["verb"] == control["verb"]), None
    )
    if control_entry is None:
        errors.append(
            f"known-positive control {control['verb']!r} is absent; census is void"
        )
    elif control_entry["classification"] != control["required_classification"]:
        errors.append(
            f"known-positive control {control['verb']!r} classified as "
            f"{control_entry['classification']!r}, expected "
            f"{control['required_classification']!r}; census is void"
        )
    elif not any(
        path["classification"] == "WRITER"
        for path in control_entry["writer_paths"]
    ):
        errors.append(
            f"known-positive control {control['verb']!r} has no WRITER path; "
            "census is void"
        )

    if errors:
        return _void_report(checked, errors, warnings, observed_revision)

    counts = {classification: 0 for classification in CLASSIFICATIONS}
    for entry in entries:
        counts[entry["classification"]] += 1
    population = sorted((entry["pack"], entry["verb"]) for entry in entries)
    population_hash = hashlib.sha256(
        canonical_json(population).encode("utf-8")
    ).hexdigest()
    return {
        "control": {
            "classification": control_entry["classification"],
            "status": "PASS",
            "verb": control["verb"],
        },
        "entries": entries,
        "observed_revision": observed_revision,
        "pack_set": sorted(checked["pack_set"]),
        "population_sha256": population_hash,
        "schema_version": REPORT_SCHEMA,
        "source_revision": checked["source_revision"],
        "status": "OK",
        "summary": {"counts": counts, "total": len(entries)},
        "warnings": sorted(set(warnings)),
    }


def _load_json(path: Path) -> Any:
    with path.open(encoding="utf-8") as handle:
        return json.load(handle)


def _kkernel_revision(path: Path) -> str:
    completed = subprocess.run(
        [str(path), "--version"],
        check=False,
        capture_output=True,
        text=True,
    )
    version = completed.stdout.strip()
    match = re.search(r"\(revision ([0-9a-f]{40}),", version)
    if completed.returncode != 0 or match is None:
        detail = completed.stderr.strip() or version
        raise CensusError(f"{path} --version has no exact revision: {detail}")
    return match.group(1)


def _inventory_from_kkernel(path: Path) -> tuple[Any, str]:
    revision = _kkernel_revision(path)
    completed = subprocess.run(
        [str(path), "pack", "list"],
        check=False,
        capture_output=True,
        text=True,
    )
    if completed.returncode != 0:
        detail = completed.stderr.strip() or completed.stdout.strip()
        raise CensusError(
            f"{path} pack list exited {completed.returncode}: {detail}"
        )
    try:
        return json.loads(completed.stdout), revision
    except json.JSONDecodeError as error:
        raise CensusError(f"{path} pack list returned invalid JSON: {error}") from error


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Compare a pinned writer-path manifest with an exact kkernel pack "
            "inventory. Output is canonical JSON and contains no timestamps."
        )
    )
    parser.add_argument("--manifest", type=Path, default=DEFAULT_MANIFEST)
    parser.add_argument(
        "--repo-root",
        type=Path,
        default=DEFAULT_REPO_ROOT,
        help="Git repository containing the manifest's pinned source revision",
    )
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument(
        "--inventory",
        type=Path,
        help="JSON captured from `kkernel pack list`",
    )
    parser.add_argument(
        "--inventory-revision",
        help="40-hex revision of the artifact that produced --inventory",
    )
    source.add_argument(
        "--kkernel",
        type=Path,
        help="exact kkernel artifact to invoke as `<path> pack list`",
    )
    parser.add_argument("--output", type=Path)
    args = parser.parse_args(argv)
    if args.inventory is not None and args.inventory_revision is None:
        parser.error("--inventory requires --inventory-revision")
    if args.kkernel is not None and args.inventory_revision is not None:
        parser.error("--inventory-revision is only valid with --inventory")
    return args


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    try:
        manifest = _load_json(args.manifest)
        if args.inventory is not None:
            observed = _load_json(args.inventory)
            observed_revision = args.inventory_revision
        else:
            observed, observed_revision = _inventory_from_kkernel(args.kkernel)
        report = build_report(
            manifest,
            observed,
            repo_root=args.repo_root,
            observed_revision=observed_revision,
        )
    except (CensusError, OSError, json.JSONDecodeError) as error:
        report = {
            "errors": [str(error)],
            "schema_version": REPORT_SCHEMA,
            "status": "VOID",
        }
    rendered = canonical_json(report)
    if args.output is None:
        sys.stdout.write(rendered)
    else:
        args.output.write_text(rendered, encoding="utf-8")
    return 0 if report["status"] == "OK" else 2


if __name__ == "__main__":
    raise SystemExit(main())
