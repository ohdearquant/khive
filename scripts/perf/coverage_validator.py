#!/usr/bin/env python3
"""Flagship coverage validator (bench-overhaul PR 1, stdlib-only).

Reads `scripts/perf/flagship_workloads.toml` (the declarative flagship
workload manifest) plus a directory of flagship-e2e ledger records
(`*.jsonl` files, one JSON object per line, `suite == "flagship-e2e"` -
the shape a checked-out `perf-data` branch's `bench-data/` directory would
hold once PR 2+ start writing them) and reports, per manifest scenario,
exactly one of five statuses:

  measured        latest matching record is schema-valid, `status: "ok"`
                  (which now also requires - for `"ok"` records only,
                  khive#945 M3 - non-empty distributions whose `successes`
                  clear `max(flagship_schema.TIMED_MIN_SUCCESSES, scenario
                  min_successes)` - raise-only, 100-success global floor,
                  khive#945 amendment §2.1 - error/timeout accounting
                  fields, and `daemon_fallback_count == 0`), measured
                  through the manifest's declared surface, its
                  `fixture_hash` matches the manifest, its `artifact.name`/
                  `artifact.sha256` are well-formed, it is within the
                  scenario's freshness window, and (when `--artifacts-dir`
                  is supplied) its raw artifact resolves inside that
                  directory and exists with a matching sha256.
  stale           a measured-quality record exists but its age exceeds the
                  freshness window for the scenario's `runner_class`
                  (OQ7 ruling: 14 days self-hosted, 7 days hosted).
  missing         no record for this `scenario_id` exists at all.
  wrong-surface   the latest record's `runtime.surface` does not match the
                  manifest's declared `surface` for this scenario.
  confounded      the latest record exists but cannot be trusted as a
                  measurement: its own `status` is `"confounded"`,
                  `"error"`, or `"insufficient_samples"` (a legitimately
                  schema-valid state - non-`"ok"` records are exempt from
                  the evidence requirements above precisely so honest
                  failure evidence stays schema-valid without ever
                  counting as measured), its `workload.fixture_hash` does
                  not match the manifest's `fixture_hash`, it fails
                  `flagship_schema.validate_record`, its `successes` fall
                  below a scenario-declared `min_successes` raise, or its
                  raw artifact is absent/invalid, escapes `--artifacts-dir`,
                  is missing, or sha256-mismatches.

Each scenario's report entry also carries `artifact_verification`, one of
`"verified"`, `"hash_mismatch"`, `"missing"`, `"unverified"`, or `None`
(only set once a scenario clears every other "measured" check).
`"unverified"` means the raw artifact path was not locally resolvable in
this run (no `--artifacts-dir` given, e.g. a CI run that only fetched the
`*.jsonl` summaries) - it is recorded explicitly rather than silently
passing as verified (khive#945 item 4). An absent/malformed `artifact.name`
or `artifact.sha256`, or a `name` that resolves outside `--artifacts-dir`
(`../` traversal or an absolute path), is reported `"missing"` instead -
never `"unverified"` - and downgrades the scenario straight to confounded
(khive#945 M1/M2).

No manifest scenario is ever reported "measured" when the records
directory is absent or empty - the coverage percentage is 0% in that case,
by construction (every scenario starts in the "missing" bucket and nothing
promotes it without a matching record). This is the PR 1 gate from
`.khive/workspaces/20260711/bench-overhaul/DESIGN.md`: "coverage validator
reports the expected scenario set and intentionally reports 0% measured
until records exist."

This script performs no benchmark execution and adds no CI wiring - it
only reads the manifest and an existing records directory.
"""

from __future__ import annotations

import argparse
import datetime
import hashlib
import json
import pathlib
import re
import sys
import tomllib

REPO_ROOT = pathlib.Path(__file__).parent.parent.parent
DEFAULT_MANIFEST = pathlib.Path(__file__).parent / "flagship_workloads.toml"

sys.path.insert(0, str(pathlib.Path(__file__).parent))
import flagship_schema  # noqa: E402  (path insert must precede this)

REQUIRED_SCENARIO_FIELDS = (
    "scenario_id",
    "feature",
    "surface",
    "operation",
    "fixture",
    "fixture_hash",
    "scale",
    "embedder",
    "state",
    "settle",
    "attempts",
    "concurrency",
    "required_percentiles",
    "request_deadline_ms",
    "request_deadline_provenance",
    "runner_class",
)

FIXTURE_HASH_RE = re.compile(r"^sha256:[0-9a-f]{64}$")
# f<N>.<verb>.<arm>.<embedder>, at least 3 dot-separated segments after the
# feature prefix - <verb> itself may be a pack-prefixed verb name that
# already contains a dot (e.g. "knowledge.compose", "brain.auto_feedback"),
# so this intentionally does not pin the segment count to exactly 4.
SCENARIO_ID_RE = re.compile(r"^f(10|[1-9])(\.[a-z0-9_]+){3,}$")

STATUSES = ("measured", "stale", "missing", "wrong-surface", "confounded")

# OQ7 ruling: freshness window is a function of runner_class, not a single
# global constant - self-hosted real-embedder/CPU runs are expensive and
# scheduled less often than hosted-hash smoke runs, so they get a longer
# grace period before a scenario is reported stale.
FRESHNESS_DAYS_HOSTED = 7
FRESHNESS_DAYS_SELF_HOSTED = 14
HOSTED_RUNNER_CLASSES = {"hosted_hash"}

AVAILABLE_OPERATIONS_BY_SURFACE = {
    "mcp_daemon": frozenset(
        {
            "context",
            "create",
            "delete",
            "get",
            "link",
            "list",
            "merge",
            "neighbors",
            "propose",
            "query",
            "resolve",
            "review",
            "search",
            "stats",
            "traverse",
            "update",
            "verbs",
            "whoami",
            "withdraw",
        }
    ),
    "admin_cli": frozenset(),
    "raw_daemon_control": frozenset({"probe_only"}),
}

# The manifest declares no `manifest_version`/`manifest_hash` field of its
# own (checked: `flagship_workloads.toml` has no such key), so the current
# manifest's identity is derived here rather than read - MANIFEST_VERSION is
# this manifest format's own version track (bumped only on a structural
# manifest revision, mirroring flagship_schema.SCHEMA_VERSION), and the hash
# is a content fingerprint of the canonicalized scenario set so any scenario
# addition/removal/edit changes the identity a record's workload must match.
MANIFEST_VERSION = "1"


def compute_manifest_hash(scenarios: list[dict]) -> str:
    """Deterministic sha256 fingerprint of a manifest's scenario set, stable
    across re-parses regardless of on-disk declaration order."""
    ordered = sorted(scenarios, key=lambda sc: sc.get("scenario_id", ""))
    canonical = json.dumps(ordered, sort_keys=True, default=str)
    digest = hashlib.sha256(canonical.encode("utf-8")).hexdigest()
    return f"sha256:{digest}"


def current_manifest_identity(manifest: dict) -> tuple[str, str]:
    """(manifest_version, manifest_hash) for the manifest as currently
    loaded - what a fresh record's `workload.manifest_version`/
    `workload.manifest_hash` must match to count as measuring this
    manifest revision, not a stale one."""
    return MANIFEST_VERSION, compute_manifest_hash(manifest.get("scenario", []))


class ManifestError(Exception):
    """A structural problem with the manifest itself (duplicate id, bad
    enum, malformed hash, missing field) - distinct from a per-scenario
    coverage status, which assumes the manifest is already well-formed."""


def load_manifest(path: pathlib.Path) -> tuple[dict, list[str]]:
    """Parse and structurally validate the manifest. Returns (data, errors).
    `errors` is non-empty for duplicate scenario_ids, invalid `surface`
    values, operations unavailable on their declared surface, malformed
    `fixture_hash` values, or missing required fields - the manifest is still
    returned (best-effort) so callers can inspect what did parse even when
    validation flags a problem.
    """
    with path.open("rb") as fh:
        data = tomllib.load(fh)

    errors: list[str] = []
    scenarios = data.get("scenario", [])
    if not scenarios:
        errors.append("manifest declares zero [[scenario]] entries")

    seen_ids: dict[str, int] = {}
    for idx, sc in enumerate(scenarios):
        label = sc.get("scenario_id", f"<entry {idx}>")

        missing_fields = [f for f in REQUIRED_SCENARIO_FIELDS if f not in sc]
        if missing_fields:
            errors.append(f"{label}: missing required field(s): {', '.join(missing_fields)}")
            continue

        if sc["scenario_id"] in seen_ids:
            errors.append(
                f"duplicate scenario_id {sc['scenario_id']!r} (entries {seen_ids[sc['scenario_id']]} and {idx})"
            )
        else:
            seen_ids[sc["scenario_id"]] = idx

        if not SCENARIO_ID_RE.match(sc["scenario_id"]):
            errors.append(
                f"{label}: scenario_id does not match f<N>.<verb>.<arm>.<embedder> convention: {sc['scenario_id']!r}"
            )

        if sc["feature"] not in flagship_schema.MANIFEST_FEATURES:
            errors.append(
                f"{label}: invalid feature {sc['feature']!r} (expected one of {flagship_schema.MANIFEST_FEATURES})"
            )

        surface = sc["surface"]
        operation = sc["operation"]
        if not isinstance(surface, str):
            errors.append(f"{label}: surface must be a string, got {type(surface).__name__}: {surface!r}")
        elif surface not in flagship_schema.SURFACES:
            errors.append(f"{label}: invalid surface {surface!r} (expected one of {flagship_schema.SURFACES})")

        if not isinstance(operation, str):
            errors.append(f"{label}: operation must be a string, got {type(operation).__name__}: {operation!r}")
        elif isinstance(surface, str):
            available_operations = AVAILABLE_OPERATIONS_BY_SURFACE.get(surface)
            if available_operations is not None and operation not in available_operations:
                available = ", ".join(sorted(available_operations)) or "none"
                errors.append(
                    f"{label}: operation is not available on {surface}: {operation!r} "
                    f"(available: {available})"
                )

        if sc["embedder"] not in flagship_schema.EMBEDDERS:
            errors.append(f"{label}: invalid embedder {sc['embedder']!r} (expected one of {flagship_schema.EMBEDDERS})")

        if sc["state"] not in flagship_schema.STATES:
            errors.append(f"{label}: invalid state {sc['state']!r} (expected one of {flagship_schema.STATES})")

        if sc["runner_class"] not in flagship_schema.RUNNER_CLASSES:
            errors.append(
                f"{label}: invalid runner_class {sc['runner_class']!r} (expected one of {flagship_schema.RUNNER_CLASSES})"
            )

        if not FIXTURE_HASH_RE.match(str(sc["fixture_hash"])):
            errors.append(f"{label}: fixture_hash must match 'sha256:<64 lowercase hex>', got {sc['fixture_hash']!r}")

        if not isinstance(sc["attempts"], int) or sc["attempts"] <= 0:
            errors.append(f"{label}: attempts must be a positive integer, got {sc['attempts']!r}")

        if not isinstance(sc["concurrency"], int) or sc["concurrency"] <= 0:
            errors.append(f"{label}: concurrency must be a positive integer, got {sc['concurrency']!r}")

        if not isinstance(sc["required_percentiles"], list) or not sc["required_percentiles"]:
            errors.append(f"{label}: required_percentiles must be a non-empty array")

        if "min_successes" in sc and (
            isinstance(sc["min_successes"], bool)
            or not isinstance(sc["min_successes"], int)
            or sc["min_successes"] <= 0
        ):
            errors.append(f"{label}: min_successes must be a positive integer, got {sc['min_successes']!r}")

    return data, errors


def _required_scenario_ids_by_feature(scenarios: list[dict]) -> dict[str, list[str]]:
    by_feature: dict[str, list[str]] = {feature: [] for feature in flagship_schema.MANIFEST_FEATURES}
    for sc in scenarios:
        by_feature.setdefault(sc.get("feature", "?"), []).append(sc.get("scenario_id", "?"))
    return by_feature


def load_records(records_dir: pathlib.Path | None) -> list[dict]:
    """Load every JSONL record with `suite == "flagship-e2e"` from every
    `*.jsonl` file directly under `records_dir`. Returns an empty list if
    `records_dir` is None or does not exist - the "0% measured with no
    records" property falls out of this returning [] rather than raising.
    """
    if records_dir is None or not records_dir.exists():
        return []
    records: list[dict] = []
    for jsonl_path in sorted(records_dir.glob("*.jsonl")):
        for line in jsonl_path.read_text().splitlines():
            line = line.strip()
            if not line:
                continue
            try:
                rec = json.loads(line)
            except json.JSONDecodeError:
                continue
            if rec.get("suite") == flagship_schema.SUITE_NAME:
                records.append(rec)
    return records


def _parse_timestamp(ts: str) -> datetime.datetime:
    return datetime.datetime.fromisoformat(ts.replace("Z", "+00:00"))


def _latest_record(records: list[dict], scenario_id: str) -> dict | None:
    matches = [r for r in records if r.get("scenario_id") == scenario_id]
    if not matches:
        return None

    def _key(r: dict) -> datetime.datetime:
        try:
            return _parse_timestamp(r.get("timestamp", ""))
        except ValueError:
            return datetime.datetime.min.replace(tzinfo=datetime.timezone.utc)

    return max(matches, key=_key)


def _freshness_window_days(runner_class: str) -> int:
    return FRESHNESS_DAYS_HOSTED if runner_class in HOSTED_RUNNER_CLASSES else FRESHNESS_DAYS_SELF_HOSTED


ARTIFACT_VERIFICATION_STATES = ("verified", "hash_mismatch", "missing", "unverified")


def verify_artifact(record: dict, artifacts_dir: pathlib.Path | None) -> tuple[str, str]:
    """Resolve a measured record's `artifact.name` against `artifacts_dir` and
    check existence + sha256. Returns (verification, reason).

    `artifacts_dir` is the local directory a raw artifact file would be
    checked out into (distinct from `--records-dir`, which holds the
    summary `*.jsonl` ledger records). When `artifacts_dir` is None the raw
    artifact path is not resolvable in this run (the common CI shape, where
    only the jsonl summaries are fetched) - that is recorded as
    `"unverified"`, not silently treated as passing, per khive#945 item 4.

    `artifact.name`/`artifact.sha256` are checked for well-formedness
    (non-empty name, 64-lowercase-hex sha256) before that `"unverified"`
    fallback is ever reached (khive#945 M1) - an absent or invalid digest
    downgrades straight to `"missing"` (which `compute_coverage` treats as
    confounding) even when no `--artifacts-dir` was supplied.

    The candidate path is resolved and required to remain below the
    resolved `artifacts_dir` (khive#945 M2) - a `name` containing `../`
    segments or naming an absolute path that escapes `artifacts_dir` is
    treated as `"missing"` rather than read, so a record can never certify
    an unrelated file outside the supplied directory as its evidence."""
    artifact = record.get("artifact", {})
    name = artifact.get("name") if isinstance(artifact, dict) else None
    expected_sha = artifact.get("sha256") if isinstance(artifact, dict) else None

    if not isinstance(name, str) or not name:
        return "missing", "record's artifact.name is empty or invalid; no raw artifact path to resolve"
    if not isinstance(expected_sha, str) or not flagship_schema.ARTIFACT_SHA256_RE.match(expected_sha):
        return (
            "missing",
            f"record's artifact.sha256 {expected_sha!r} is not a valid 64-character lowercase hex digest",
        )

    if artifacts_dir is None:
        return "unverified", "no --artifacts-dir supplied; raw artifact existence/hash was not checked locally"

    resolved_dir = artifacts_dir.resolve()
    candidate = (artifacts_dir / name).resolve()
    try:
        candidate.relative_to(resolved_dir)
    except ValueError:
        return (
            "missing",
            f"record's artifact.name {name!r} resolves to {candidate}, outside --artifacts-dir {resolved_dir}; refusing to read",
        )

    if not candidate.is_file():
        return "missing", f"referenced raw artifact not found at {candidate}"

    actual_sha = hashlib.sha256(candidate.read_bytes()).hexdigest()
    if actual_sha != expected_sha:
        return (
            "hash_mismatch",
            f"raw artifact at {candidate} hashes to {actual_sha!r}, record declares artifact.sha256 {expected_sha!r}",
        )

    return "verified", f"raw artifact verified at {candidate}"


def _cohort_mismatches(scenario: dict, record: dict) -> list[str]:
    """Compare the manifest scenario's cohort-defining fields against the
    record's actual measured cohort. A record whose surface/fixture_hash
    match but whose runner tier, embedder, settle posture, scale, or
    concurrency/attempts differ measured a different population than the
    scenario declares - it cannot stand in for the required scenario, even
    though it is otherwise schema-valid and fresh."""
    mismatches: list[str] = []
    runtime = record.get("runtime", {})
    settle = record.get("settle", {})
    workload = record.get("workload", {})

    if runtime.get("runner_class") != scenario["runner_class"]:
        mismatches.append(
            f"runtime.runner_class mismatch: manifest {scenario['runner_class']!r} vs record {runtime.get('runner_class')!r}"
        )
    if runtime.get("embedder") != scenario["embedder"]:
        mismatches.append(f"runtime.embedder mismatch: manifest {scenario['embedder']!r} vs record {runtime.get('embedder')!r}")
    if settle.get("state") != scenario["state"]:
        mismatches.append(f"settle.state mismatch: manifest {scenario['state']!r} vs record {settle.get('state')!r}")
    if settle.get("method") != scenario["settle"]:
        mismatches.append(f"settle.method mismatch: manifest {scenario['settle']!r} vs record {settle.get('method')!r}")
    if workload.get("scale") != scenario["scale"]:
        mismatches.append(f"workload.scale mismatch: manifest {scenario['scale']!r} vs record {workload.get('scale')!r}")
    if workload.get("concurrency") != scenario["concurrency"]:
        mismatches.append(
            f"workload.concurrency mismatch: manifest {scenario['concurrency']!r} vs record {workload.get('concurrency')!r}"
        )
    if workload.get("attempts") != scenario["attempts"]:
        mismatches.append(
            f"workload.attempts mismatch: manifest {scenario['attempts']!r} vs record {workload.get('attempts')!r}"
        )
    return mismatches


def scenario_status(
    scenario: dict,
    records: list[dict],
    now: datetime.datetime,
    manifest_version: str | None = None,
    manifest_hash: str | None = None,
) -> tuple[str, str]:
    """Return (status, reason) for one manifest scenario. `status` is one of
    STATUSES. `reason` is a short human-readable explanation.

    `manifest_version`/`manifest_hash` are the CURRENT manifest's identity
    (see `current_manifest_identity`) - when supplied, a record measuring a
    different manifest revision is confounded even if its `fixture_hash`
    still happens to match. `compute_coverage` always supplies both; the
    parameters default to `None` (check skipped) only for direct unit-test
    callers that construct scenario/record fixtures without a real
    manifest."""
    latest = _latest_record(records, scenario["scenario_id"])
    if latest is None:
        return "missing", "no matching record in the records directory"

    schema_errors = flagship_schema.validate_record(latest)
    if schema_errors:
        return "confounded", f"latest record fails schema validation: {schema_errors[0]}"

    if latest.get("status") in ("confounded", "error", "insufficient_samples"):
        return "confounded", f"latest record's own status is {latest.get('status')!r}"

    record_surface = latest.get("runtime", {}).get("surface")
    if record_surface != scenario["surface"]:
        return "wrong-surface", f"manifest declares {scenario['surface']!r}, record measured {record_surface!r}"

    record_operation = latest.get("operation")
    if record_operation != scenario["operation"]:
        return "confounded", f"operation mismatch: manifest {scenario['operation']!r} vs record {record_operation!r}"

    record_workload_scenario_id = latest.get("workload", {}).get("scenario_id")
    if record_workload_scenario_id != scenario["scenario_id"]:
        return (
            "confounded",
            f"workload.scenario_id mismatch: manifest {scenario['scenario_id']!r} vs record {record_workload_scenario_id!r}",
        )

    record_fixture_hash = latest.get("workload", {}).get("fixture_hash")
    if record_fixture_hash != scenario["fixture_hash"]:
        return "confounded", f"fixture_hash mismatch: manifest {scenario['fixture_hash']!r} vs record {record_fixture_hash!r}"

    if manifest_version is not None:
        record_manifest_version = latest.get("workload", {}).get("manifest_version")
        if record_manifest_version != manifest_version:
            return (
                "confounded",
                f"manifest_version mismatch: current {manifest_version!r} vs record {record_manifest_version!r}",
            )

    if manifest_hash is not None:
        record_manifest_hash = latest.get("workload", {}).get("manifest_hash")
        if record_manifest_hash != manifest_hash:
            return (
                "confounded",
                f"manifest_hash mismatch: current {manifest_hash!r} vs record {record_manifest_hash!r}",
            )

    cohort_mismatches = _cohort_mismatches(scenario, latest)
    if cohort_mismatches:
        return "confounded", "; ".join(cohort_mismatches)

    declared_min_successes = scenario.get("min_successes")
    if declared_min_successes is not None:
        # load_manifest rejects malformed values, but a programmatic caller can
        # bypass it - a bad declaration must confound, never raise.
        if (
            isinstance(declared_min_successes, bool)
            or not isinstance(declared_min_successes, int)
            or declared_min_successes <= 0
        ):
            return (
                "confounded",
                f"scenario declares malformed min_successes {declared_min_successes!r} (must be a positive integer)",
            )
        required_successes = max(flagship_schema.TIMED_MIN_SUCCESSES, declared_min_successes)
        for dist_name, dist in latest.get("distributions", {}).items():
            successes = dist.get("successes") if isinstance(dist, dict) else None
            if isinstance(successes, int) and successes < required_successes:
                return (
                    "confounded",
                    f"distributions.{dist_name}.successes {successes} is below the scenario's required minimum "
                    f"{required_successes} (max of the {flagship_schema.TIMED_MIN_SUCCESSES}-success floor and the "
                    f"scenario's declared min_successes {declared_min_successes!r} - raise-only, never lowers the floor)",
                )

    try:
        record_time = _parse_timestamp(latest["timestamp"])
    except (KeyError, ValueError):
        return "confounded", "latest record has an unparseable timestamp"

    window_days = _freshness_window_days(scenario["runner_class"])
    age_days = (now - record_time).total_seconds() / 86400.0
    if age_days > window_days:
        return "stale", f"latest record is {age_days:.1f} days old, exceeds the {window_days}-day freshness window"

    return "measured", f"latest record is {age_days:.1f} days old (within the {window_days}-day freshness window)"


def compute_coverage(
    manifest: dict,
    records: list[dict],
    now: datetime.datetime,
    artifacts_dir: pathlib.Path | None = None,
) -> dict:
    scenarios = manifest.get("scenario", [])
    manifest_version, manifest_hash = current_manifest_identity(manifest)
    per_scenario = []
    counts = dict.fromkeys(STATUSES, 0)
    for sc in scenarios:
        status, reason = scenario_status(sc, records, now, manifest_version, manifest_hash)
        artifact_verification = None
        if status == "measured":
            latest = _latest_record(records, sc["scenario_id"])
            artifact_verification, artifact_reason = verify_artifact(latest, artifacts_dir)
            if artifact_verification in ("missing", "hash_mismatch"):
                status = "confounded"
                reason = artifact_reason
        counts[status] += 1
        per_scenario.append(
            {
                "scenario_id": sc["scenario_id"],
                "feature": sc["feature"],
                "surface": sc["surface"],
                "runner_class": sc["runner_class"],
                "status": status,
                "reason": reason,
                "artifact_verification": artifact_verification,
            }
        )

    total = len(scenarios)
    percent_measured = (counts["measured"] / total * 100.0) if total else 0.0
    by_feature = _required_scenario_ids_by_feature(scenarios)
    features_with_zero_measured = sorted(
        feature
        for feature in flagship_schema.MANIFEST_FEATURES
        if by_feature.get(feature)
        and not any(s["feature"] == feature and s["status"] == "measured" for s in per_scenario)
    )

    return {
        "total_scenarios": total,
        "percent_measured": round(percent_measured, 2),
        "counts": counts,
        "scenarios": per_scenario,
        "features_with_zero_measured": features_with_zero_measured,
    }


def render_markdown(report: dict) -> str:
    lines = ["# Flagship coverage report", ""]
    lines.append(f"- total scenarios: {report['total_scenarios']}")
    lines.append(f"- measured: {report['percent_measured']}%")
    for status in STATUSES:
        lines.append(f"  - {status}: {report['counts'][status]}")
    if report["features_with_zero_measured"]:
        lines.append(f"- features with zero measured scenarios: {', '.join(report['features_with_zero_measured'])}")
    lines.append("")
    lines.append("| scenario_id | feature | status | reason |")
    lines.append("|---|---|---|---|")
    for sc in report["scenarios"]:
        lines.append(f"| {sc['scenario_id']} | {sc['feature']} | {sc['status']} | {sc['reason']} |")
    lines.append("")
    return "\n".join(lines)


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--manifest", default=str(DEFAULT_MANIFEST), help="path to flagship_workloads.toml")
    ap.add_argument(
        "--records-dir",
        default=None,
        help="directory of flagship-e2e *.jsonl records (e.g. a checked-out perf-data/bench-data/); "
        "omit or point at a nonexistent path to get the 0%%-measured report",
    )
    ap.add_argument("--now", default=None, help="ISO8601 timestamp to use as 'now' (default: current UTC time)")
    ap.add_argument(
        "--artifacts-dir",
        default=None,
        help="directory of raw per-record artifact files (referenced by a record's artifact.name); "
        "when supplied, a 'measured' scenario's raw artifact must exist and sha256-match or it is "
        "downgraded to 'confounded'; when omitted, artifact_verification is reported as 'unverified' "
        "rather than silently passing (e.g. CI runs that only fetch the *.jsonl summaries)",
    )
    ap.add_argument("--format", choices=["json", "markdown"], default="markdown")
    ap.add_argument("--out", help="write the report to this path instead of stdout")
    ap.add_argument(
        "--strict",
        action="store_true",
        help="exit nonzero if the manifest itself fails structural validation "
        "(duplicate ids, invalid enums, unavailable operations, malformed fixture hashes, missing fields)",
    )
    args = ap.parse_args(argv)

    manifest_path = pathlib.Path(args.manifest)
    manifest, manifest_errors = load_manifest(manifest_path)
    if manifest_errors:
        for err in manifest_errors:
            print(f"[coverage_validator] manifest error: {err}", file=sys.stderr)
        if args.strict:
            return 1

    now = _parse_timestamp(args.now) if args.now else datetime.datetime.now(datetime.timezone.utc)
    records_dir = pathlib.Path(args.records_dir) if args.records_dir else None
    records = load_records(records_dir)
    artifacts_dir = pathlib.Path(args.artifacts_dir) if args.artifacts_dir else None
    report = compute_coverage(manifest, records, now, artifacts_dir)
    report["manifest_errors"] = manifest_errors

    if args.format == "json":
        out = json.dumps(report, indent=2, sort_keys=True)
    else:
        out = render_markdown(report)

    if args.out:
        pathlib.Path(args.out).write_text(out)
        print(f"[coverage_validator] wrote report -> {args.out}")
    else:
        print(out)

    return 0


if __name__ == "__main__":
    sys.exit(main())
