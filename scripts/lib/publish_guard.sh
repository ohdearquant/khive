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
    local name found c members

    # Capture the producer's output AND its exit status explicitly. A process
    # substitution's exit status is not propagated to a `while` loop, so a
    # failed parser (invalid/corrupt metadata, missing python3) would yield no
    # names and let the guard pass silently. A publish preflight must fail
    # CLOSED when it cannot enumerate members (#1071 review).
    if ! members=$(publishable_members_from_metadata < "$metadata_path"); then
        echo "ERROR: could not enumerate publishable workspace members from cargo metadata (parser failed) — refusing to proceed" >&2
        return 2
    fi

    while IFS= read -r name; do
        [[ -n "$name" ]] || continue
        found=false
        for c in "${ladder[@]}"; do
            if [[ "$c" == "$name" ]]; then
                found=true
                break
            fi
        done
        $found || missing+=("$name")
    done <<< "$members"

    if [[ ${#missing[@]} -gt 0 ]]; then
        echo "ERROR: publishable workspace member(s) missing from the CRATES ladder in scripts/publish.sh:" >&2
        printf '  - %s\n' "${missing[@]}" >&2
        return 1
    fi
    return 0
}

# publish_verify_mode <value of NO_VERIFY>
# Resolves the NO_VERIFY knob for the live ladder: prints `verify` (unset or
# "0", the default) or `no-verify` ("1"), and fails on any other value so a
# typo cannot silently choose a mode. `no-verify` makes each live
# `cargo publish` skip its verification build; use it only when
# `cargo publish --dry-run --locked --workspace` has already verified every
# crate at the same commit, so the publishing machine only packages and
# uploads.
publish_verify_mode() {
    case "${1-}" in
        "" | 0) echo verify ;;
        1) echo no-verify ;;
        *)
            echo "ERROR: NO_VERIFY must be unset, 0 or 1 (got '$1')" >&2
            return 1
            ;;
    esac
}
