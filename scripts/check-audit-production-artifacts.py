#!/usr/bin/env python3
"""Require Cargo evidence that the audit tests used the ordinary runtime library."""

import json
import pathlib
import sys


def check_artifacts(lines):
    seen = False
    for line in lines:
        if not line.startswith("{"):
            continue
        event = json.loads(line)
        target = event.get("target", {})
        if (
            event.get("reason") != "compiler-artifact"
            or target.get("name") != "khive_runtime"
            or "lib" not in target.get("kind", [])
        ):
            continue
        seen = True
        if event.get("profile", {}).get("test") is not False:
            raise ValueError("runtime artifact must be an ordinary dependency, not a test target")
        features = event.get("features")
        if not isinstance(features, list) or any(not isinstance(f, str) for f in features):
            raise ValueError("runtime artifact must report its enabled features")
        forbidden = {"test-internals", "fault-injection"}.intersection(features)
        if forbidden:
            raise ValueError("runtime enabled test-only features: " + ", ".join(sorted(forbidden)))
    if not seen:
        raise ValueError("Cargo emitted no khive_runtime library artifact")


if __name__ == "__main__":
    try:
        with pathlib.Path(sys.argv[1]).open() as source:
            check_artifacts(source)
    except (ValueError, OSError, IndexError) as error:
        sys.exit(str(error))
    print("audit tests used the ordinary runtime library")
