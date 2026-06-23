#!/usr/bin/env bash
# apply-autonomous-merge.sh
#
# Activates (or previews) the ADR-066 autonomous-merge configuration on
# ohdearquant/khive: the gate wall becomes the reviewer, release becomes the
# human gate. NOT auto-run. Ocean runs this manually at activation (the HC-7
# person->automation step), in STEP order, after the gate PRs are on main.
#
# MECHANISM: main is gated by repository RULESET 17362266, not by classic
# branch protection (GET .../branches/main/protection returns 404 on this
# repo). This script edits that ruleset in place via jq, preserving its
# bypass_actors and conditions; it never writes classic protection (doing so
# would create a second, conflicting gate).
#
# STEPS (run in this exact order — each refuses to run out of order):
#
#   STEP=gates        (default)
#     - Adds the full required-status-check battery (REQUIRED_CONTEXTS below)
#       to the ruleset's required_status_checks rule.
#     - Enables repo auto-merge + delete-branch-on-merge.
#     - LEAVES required_approving_review_count at its current value (1). The
#       per-PR human review gate stays UP while the wall goes up. This is the
#       safe direction: adding gates never opens a hole.
#     - Preflight: every required context must already report on main's HEAD
#       commit, or it aborts (a required check that never reports locks every
#       PR forever).
#
#   STEP=release-gate
#     - Creates the 'publish' environment with the running admin (Ocean) as a
#       required reviewer. This gates release.yml's publish-all job: even an
#       automated or accidental tag pauses until a human approves the
#       deployment. Safe; idempotent.
#
#   STEP=autonomy
#     - THE FLIP. Sets required_approving_review_count=0 on the ruleset's
#       pull_request rule (require_code_owner_review stays false). This removes
#       the last per-PR human gate: the wall becomes the sole reviewer.
#     - Refuses unless BOTH prior steps are already in effect: it verifies the
#       ruleset already carries every required context AND the 'publish'
#       environment exists with >=1 required reviewer. You cannot flip to zero
#       approvals before the wall is up and the release gate is in place.
#     - This is the hard-to-reverse, HC-7-gated step. Run it LAST, only on
#       Ocean's explicit go.
#
# DRY_RUN (default ON): prints the exact mutations and exits 0 without changing
#   anything. To apply:  DRY_RUN=false STEP=<step> ./scripts/apply-autonomous-merge.sh
#
# Prerequisites: gh CLI authenticated as a repo admin (in the ruleset's
#   bypass_actors), and jq on PATH.

set -euo pipefail

REPO="ohdearquant/khive"
RULESET_ID=17362266
BRANCH="main"
ENVIRONMENT="publish"

DRY_RUN="${DRY_RUN:-true}"
STEP="${STEP:-gates}"

# ---------------------------------------------------------------------------
# Required status check contexts (ADR-066 §1). Each string MUST equal the
# GitHub check-run name verbatim. Verified 2026-06-23 against the job `name:`
# fields:
#   ci.yml (on main):           "CI (ubuntu-latest)", "CI (macos-latest)"
#                               (job name "CI" x matrix os),
#                               "JSON/JSONL data-leak guard", "Secret scan
#                               (gitleaks)", "Docs lint",
#                               "Marketplace example validator"
#   ci.yml (lands with #215):   "Supply-chain (cargo-deny)"
#   ci.yml (lands with #216):   "SemVer checks", "Doc build (-D warnings)",
#                               "Dependency review", "Coverage ratchet"
# EXCLUDED (intentionally NOT required): the two path-filtered bench gates,
#   "ANN structural regression gate (synthetic, hermetic)" (bench-1m.yml) and
#   "Pipeline Regression Gate" (bench-pipeline.yml). They only report when a PR
#   touches their paths; requiring them would make every unrelated PR wait
#   forever on a check that never reports.
# ---------------------------------------------------------------------------
REQUIRED_CONTEXTS=(
  "CI (ubuntu-latest)"
  "CI (macos-latest)"
  "JSON/JSONL data-leak guard"
  "Secret scan (gitleaks)"
  "Docs lint"
  "Marketplace example validator"
  "Supply-chain (cargo-deny)"
  "SemVer checks"
  "Doc build (-D warnings)"
  "Dependency review"
  "Coverage ratchet"
)

require_jq() {
  command -v jq >/dev/null 2>&1 || { echo "ABORT: jq is required." >&2; exit 1; }
}

# JSON array of {context: "..."} objects for the ruleset rule.
contexts_json() {
  printf '%s\n' "${REQUIRED_CONTEXTS[@]}" \
    | jq -R . | jq -s 'map({context: .})'
}

# Names of check-runs that reported on main's HEAD commit.
main_head_check_names() {
  gh api --paginate "repos/${REPO}/commits/${BRANCH}/check-runs" \
    --jq '.check_runs[].name'
}

# Abort unless every required context reports on main's HEAD. Prevents locking
# the repo behind a required check that never runs.
preflight_contexts_report() {
  local reported missing=()
  reported="$(main_head_check_names || true)"
  local ctx
  for ctx in "${REQUIRED_CONTEXTS[@]}"; do
    grep -Fxq "${ctx}" <<<"${reported}" || missing+=("${ctx}")
  done
  if (( ${#missing[@]} > 0 )); then
    echo "ABORT: these required contexts do not report on ${BRANCH} HEAD:" >&2
    printf '  - %s\n' "${missing[@]}" >&2
    echo "Merge the PR(s) that add them first (gate wall: #215 then #216)." >&2
    echo "Requiring a check that never reports blocks every PR." >&2
    exit 1
  fi
  echo "Preflight OK: all ${#REQUIRED_CONTEXTS[@]} required contexts report on ${BRANCH}."
}

# Current ruleset, stripped to the fields the PUT accepts, with mutations
# applied by the caller-supplied jq filter.
build_ruleset_body() {
  local filter="$1"
  gh api "repos/${REPO}/rulesets/${RULESET_ID}" | jq \
    --argjson contexts "$(contexts_json)" \
    "${filter} | {name, target, enforcement, bypass_actors, conditions, rules}"
}

put_ruleset() {
  local body="$1"
  if [[ "${DRY_RUN}" != "false" ]]; then
    echo "[DRY RUN] PUT repos/${REPO}/rulesets/${RULESET_ID} with body:"
    echo "${body}" | jq .
  else
    echo "${body}" | gh api --method PUT "repos/${REPO}/rulesets/${RULESET_ID}" --input - >/dev/null
    echo "Ruleset ${RULESET_ID} updated."
  fi
}

# ---------------------------------------------------------------------------
step_gates() {
  echo "[STEP gates] Raising the gate wall (review gate stays at 1)."
  if [[ "${DRY_RUN}" != "false" ]]; then
    echo "[DRY RUN] (preflight skipped in dry run; it runs for real on apply)"
  else
    preflight_contexts_report
  fi

  # Set the required_status_checks rule's contexts to the full battery; leave
  # the pull_request rule (review count) untouched.
  local body
  body="$(build_ruleset_body '
    .rules |= map(
      if .type == "required_status_checks"
      then .parameters.required_status_checks = $contexts
      else . end
    )')"
  put_ruleset "${body}"

  echo "[STEP gates] Enabling repo auto-merge + delete-branch-on-merge."
  local settings='{"allow_auto_merge":true,"delete_branch_on_merge":true}'
  if [[ "${DRY_RUN}" != "false" ]]; then
    echo "[DRY RUN] PATCH repos/${REPO} ${settings}"
  else
    echo "${settings}" | gh api --method PATCH "repos/${REPO}" --input - >/dev/null
    echo "Repo settings updated."
  fi
}

step_release_gate() {
  echo "[STEP release-gate] Creating '${ENVIRONMENT}' environment with a required reviewer."
  local admin_id admin_login body
  admin_id="$(gh api user --jq .id)"
  admin_login="$(gh api user --jq .login)"
  body="$(jq -n --argjson id "${admin_id}" \
    '{wait_timer:0, reviewers:[{type:"User", id:$id}], deployment_branch_policy:null}')"
  if [[ "${DRY_RUN}" != "false" ]]; then
    echo "[DRY RUN] PUT repos/${REPO}/environments/${ENVIRONMENT} (required reviewer: ${admin_login}, id ${admin_id})"
    echo "${body}" | jq .
  else
    echo "${body}" | gh api --method PUT "repos/${REPO}/environments/${ENVIRONMENT}" --input - >/dev/null
    echo "Environment '${ENVIRONMENT}' configured (required reviewer: ${admin_login})."
  fi
}

# Verify the wall is up and the release gate exists before allowing the flip.
preflight_autonomy() {
  local rs have_contexts ctx missing=()
  rs="$(gh api "repos/${REPO}/rulesets/${RULESET_ID}")"
  have_contexts="$(jq -r '
    (.rules[] | select(.type=="required_status_checks")
      | .parameters.required_status_checks[].context)' <<<"${rs}" 2>/dev/null || true)"
  for ctx in "${REQUIRED_CONTEXTS[@]}"; do
    grep -Fxq "${ctx}" <<<"${have_contexts}" || missing+=("${ctx}")
  done
  if (( ${#missing[@]} > 0 )); then
    echo "ABORT: ruleset is missing required contexts — run STEP=gates first:" >&2
    printf '  - %s\n' "${missing[@]}" >&2
    exit 1
  fi

  local reviewers
  reviewers="$(gh api "repos/${REPO}/environments/${ENVIRONMENT}" \
    --jq '[.protection_rules[]? | select(.type=="required_reviewers") | .reviewers[]?] | length' \
    2>/dev/null || echo 0)"
  if [[ "${reviewers}" -lt 1 ]]; then
    echo "ABORT: '${ENVIRONMENT}' environment has no required reviewer — run STEP=release-gate first." >&2
    exit 1
  fi
  echo "Preflight OK: wall is up (all contexts required) and release gate has ${reviewers} reviewer(s)."
}

step_autonomy() {
  echo "[STEP autonomy] THE FLIP: removing the last per-PR human approval gate."
  echo "[STEP autonomy] This is hard to reverse. Ensure Ocean has explicitly approved (HC-7)."
  preflight_autonomy
  local body
  body="$(build_ruleset_body '
    .rules |= map(
      if .type == "pull_request"
      then .parameters.required_approving_review_count = 0
      else . end
    )')"
  put_ruleset "${body}"
  echo "[STEP autonomy] Done. main now merges on a green gate wall with zero required human approvals."
}

# ---------------------------------------------------------------------------
require_jq

if [[ "${DRY_RUN}" != "false" ]]; then
  echo "=== DRY RUN (STEP=${STEP}). Nothing will be mutated. ==="
  echo "=== To apply: DRY_RUN=false STEP=${STEP} $0 ==="
  echo ""
fi

case "${STEP}" in
  gates)        step_gates ;;
  release-gate) step_release_gate ;;
  autonomy)     step_autonomy ;;
  *)
    echo "ABORT: unknown STEP='${STEP}'. Use gates | release-gate | autonomy." >&2
    exit 1
    ;;
esac

if [[ "${DRY_RUN}" != "false" ]]; then
  echo ""
  echo "Activation order: STEP=gates  ->  STEP=release-gate  ->  STEP=autonomy"
  echo "Run STEP=autonomy LAST, only on Ocean's explicit go (HC-7)."
fi
