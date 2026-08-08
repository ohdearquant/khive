#!/usr/bin/env bash
# Reproduce the checked-in khive.repo.v1 golden bundle and browser asset.

set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
scratch_root="$(mktemp -d "${TMPDIR:-/tmp}/khive-repo-showcase.XXXXXX")"
golden_path="$repo_root/docs/schemas/examples/khive-repo-v1-khive.json"
schema_path="$repo_root/docs/schemas/khive-repo-v1.schema.json"

cleanup() {
  rm -rf -- "$scratch_root"
}
trap cleanup EXIT

cargo run --locked \
  --manifest-path "$repo_root/crates/Cargo.toml" \
  -p khive-repo-showcase \
  --example generate_schema -- \
  "$schema_path"

cargo run --locked \
  --manifest-path "$repo_root/crates/Cargo.toml" \
  -p kkernel -- \
  repo build \
  --enable-l2 \
  --source https://github.com/ohdearquant/khive \
  --revision c2979d2443738a075e55a170c772d1dc86cf0f91 \
  --work-dir "$scratch_root/work" \
  --include commits \
  --tags none \
  --default-branch main \
  --generated-at 2026-08-07T18:00:00Z \
  --out "$golden_path"

node "$repo_root/apps/kg-editor/scripts/sync-showcase-assets.mjs"

echo "wrote $schema_path"
echo "wrote $golden_path and synchronized browser assets"
