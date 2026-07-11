#!/usr/bin/env bash
# Single source of truth for crates with no crates.io baseline yet, so
# cargo-semver-checks has nothing to diff against. Drop a crate from this
# list once it has one published version. Consumed by scripts/publish.sh
# (SemVer preflight/live gate) and .github/workflows/release.yml (release
# SemVer gate), so both paths stay in sync.
SEMVER_EXCLUDED_CRATES=(
    khive-quant
    khive-channel
    khive-channel-email
    khive-pack-formal
    khive-pack-session
)

SEMVER_EXCLUDE_ARGS=()
for _crate in "${SEMVER_EXCLUDED_CRATES[@]}"; do
    SEMVER_EXCLUDE_ARGS+=(--exclude "$_crate")
done
unset _crate

semver_exclude_csv() {
    local IFS=,
    echo "${SEMVER_EXCLUDED_CRATES[*]}"
}
