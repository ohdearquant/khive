#!/usr/bin/env bash
# Preflight guard (#1069): every publishable cargo workspace member must
# appear in publish.sh's CRATES ladder. A member is publishable unless its
# Cargo.toml sets `publish = false`, which `cargo metadata` serializes as
# an empty `publish` array (null, or a non-empty registry list, means the
# member is publishable).
#
# Scoped to CRATES membership only — the separate cargo-semver-checks
# `--exclude` list (also in publish.sh) is a different concern and is not
# guarded here (#1069 out-of-scope).
#
# Sourced by scripts/publish.sh and by scripts/tests/publish-guard-test.sh
# so the check can run against a fixture without a live `cargo metadata`.

# Prints publishable workspace member names (one per line) from a
# `cargo metadata --no-deps --format-version=1` JSON blob on stdin.
publishable_members_from_metadata() {
    python3 -c '
import sys, json
pkgs = json.load(sys.stdin)["packages"]
for p in sorted(pkgs, key=lambda p: p["name"]):
    if p.get("publish") != []:
        print(p["name"])
'
}

# check_crates_ladder <metadata_json_path> <crate>...
# Prints and fails (non-zero) if any publishable workspace member found in
# the metadata file is absent from the given CRATES ladder.
check_crates_ladder() {
    local metadata_path="$1"
    shift
    local -a ladder=("$@")
    local -a missing=()
    local name found c

    while IFS= read -r name; do
        found=false
        for c in "${ladder[@]}"; do
            if [[ "$c" == "$name" ]]; then
                found=true
                break
            fi
        done
        $found || missing+=("$name")
    done < <(publishable_members_from_metadata < "$metadata_path")

    if [[ ${#missing[@]} -gt 0 ]]; then
        echo "ERROR: publishable workspace member(s) missing from the CRATES ladder in scripts/publish.sh:" >&2
        printf '  - %s\n' "${missing[@]}" >&2
        return 1
    fi
    return 0
}
