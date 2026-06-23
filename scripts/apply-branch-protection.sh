#!/usr/bin/env bash
# apply-branch-protection.sh
#
# Applies ADR-066 §9 branch protection to main, enables repo auto-merge,
# and enables delete-branch-on-merge.
#
# NOT auto-run. Ocean runs this manually at activation (the HC-7
# person->automation step), after the required gates are green on main.
#
# ACTIVATION ORDER (must hold, or the repo locks / the Lane-2 hold is void):
#   1. Merge the CODEOWNERS PR        -- otherwise require_code_owner_reviews
#                                        has no owner to satisfy and the
#                                        Lane-2 hold is silently absent.
#   2. Merge the deterministic-gates PR (#207) -- otherwise the
#      "Supply-chain (cargo-deny)" required context never reports and every
#      PR is blocked forever. This script preflights for it (see below).
#   3. Run this script.
#
# DRY_RUN (default ON):
#   Any invocation where DRY_RUN is unset or != "false" prints the gh api
#   calls that would be made and exits 0 without mutating anything.
#   To actually apply:
#     DRY_RUN=false ./scripts/apply-branch-protection.sh
#
# FORCE (default OFF):
#   Skips the supply-chain preflight. Only use if you have independently
#   confirmed the gate reports on main. FORCE=true DRY_RUN=false ./...
#
# Prerequisites: gh CLI authenticated as a repo admin (ohdearquant).

set -euo pipefail

REPO="ohdearquant/khive"
BRANCH="main"

DRY_RUN="${DRY_RUN:-true}"
FORCE="${FORCE:-false}"

# ---------------------------------------------------------------------------
# Required status check contexts (ADR-066 §9).
# Each string must match the job `name:` field exactly as GitHub reports it.
# Verified 2026-06-22 against main and the deterministic-gates PR (#207):
#   - "CI (ubuntu-latest)"        -- ci.yml, job ci, matrix os=ubuntu-latest
#   - "CI (macos-latest)"         -- ci.yml, job ci, matrix os=macos-latest
#   - "Supply-chain (cargo-deny)" -- ci.yml, job supply-chain (lands with #207)
#   - "JSON/JSONL data-leak guard"-- ci.yml, job data-leak-guard
#   - "Secret scan (gitleaks)"    -- ci.yml, job secret-scan
#   - "Docs lint"                 -- ci.yml, job docs
#   - "Marketplace example validator" -- ci.yml, job marketplace
# Excluded: "ANN structural regression gate (synthetic, hermetic)" (bench-1m.yml)
#           "Pipeline Regression Gate" (bench-pipeline.yml)
# Both bench workflows are path-filtered; excluding them prevents the gate
# waiting forever when a PR does not touch their paths.
# ---------------------------------------------------------------------------

CONTEXTS='[
  "CI (ubuntu-latest)",
  "CI (macos-latest)",
  "Supply-chain (cargo-deny)",
  "JSON/JSONL data-leak guard",
  "Secret scan (gitleaks)",
  "Docs lint",
  "Marketplace example validator"
]'

PROTECTION_BODY="$(cat <<EOF
{
  "required_status_checks": {
    "strict": true,
    "contexts": ${CONTEXTS}
  },
  "required_pull_request_reviews": {
    "required_approving_review_count": 0,
    "require_code_owner_reviews": true,
    "dismiss_stale_reviews": true
  },
  "enforce_admins": false,
  "required_linear_history": true,
  "allow_force_pushes": false,
  "allow_deletions": false,
  "restrictions": null
}
EOF
)"

REPO_SETTINGS_BODY='{"allow_auto_merge":true,"delete_branch_on_merge":true}'

# ---------------------------------------------------------------------------

if [[ "${DRY_RUN}" != "false" ]]; then
  echo "[DRY RUN] The following gh api calls would be executed (after a"
  echo "[DRY RUN] supply-chain preflight, unless FORCE=true)."
  echo "[DRY RUN] To apply, run: DRY_RUN=false $0"
  echo ""
  echo "--- (0) Preflight: confirm 'Supply-chain (cargo-deny)' is on ${BRANCH} ---"
  echo "gh api -H 'Accept: application/vnd.github.raw' \\"
  echo "  repos/${REPO}/contents/.github/workflows/ci.yml?ref=${BRANCH} | grep -q 'Supply-chain (cargo-deny)'"
  echo ""
  echo "--- (1) Apply branch protection to ${BRANCH} ---"
  echo "gh api --method PUT \\"
  echo "  repos/${REPO}/branches/${BRANCH}/protection \\"
  echo "  --input - <<'BODY'"
  echo "${PROTECTION_BODY}"
  echo "BODY"
  echo ""
  echo "--- (2) Enable repo auto-merge + delete-branch-on-merge ---"
  echo "gh api --method PATCH \\"
  echo "  repos/${REPO} \\"
  echo "  --input - <<'BODY'"
  echo "${REPO_SETTINGS_BODY}"
  echo "BODY"
  exit 0
fi

# ---------------------------------------------------------------------------
# Preflight (DRY_RUN=false): refuse to apply if the supply-chain gate is not
# yet on main. Applying a required context that never reports locks every PR.
# ---------------------------------------------------------------------------

if [[ "${FORCE}" != "true" ]]; then
  echo "[0/2] Preflight: checking 'Supply-chain (cargo-deny)' exists on ${BRANCH}..."
  if gh api -H "Accept: application/vnd.github.raw" \
       "repos/${REPO}/contents/.github/workflows/ci.yml?ref=${BRANCH}" \
       | grep -q "Supply-chain (cargo-deny)"; then
    echo "Preflight OK: supply-chain gate present on ${BRANCH}."
  else
    echo "ABORT: 'Supply-chain (cargo-deny)' is not on ${BRANCH} yet." >&2
    echo "Merge the deterministic-gates PR (#207) first, or applying this" >&2
    echo "protection will block every PR on a check that never reports." >&2
    echo "Override only if you are certain: FORCE=true DRY_RUN=false $0" >&2
    exit 1
  fi
fi

# ---------------------------------------------------------------------------
# Live application (DRY_RUN=false)
# ---------------------------------------------------------------------------

echo "[1/2] Applying branch protection to ${REPO} ${BRANCH}..."
echo "${PROTECTION_BODY}" | gh api --method PUT \
  "repos/${REPO}/branches/${BRANCH}/protection" \
  --input -
echo "Branch protection applied."

echo "[2/2] Enabling auto-merge and delete-branch-on-merge on ${REPO}..."
echo "${REPO_SETTINGS_BODY}" | gh api --method PATCH \
  "repos/${REPO}" \
  --input -
echo "Repo settings updated."

echo ""
echo "Done. Verify in the GitHub UI:"
echo "  https://github.com/${REPO}/settings/branches"
echo ""
echo "Confirm each of the 7 required contexts appears under branch protection"
echo "for '${BRANCH}' and that 'Require status checks to pass' is enabled."
