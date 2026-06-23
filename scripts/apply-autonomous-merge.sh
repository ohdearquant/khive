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
#     - Preflight: every required context must already have a recent PASSING run
#       on main's HEAD OR a recent PR head, or it aborts (a required check that
#       never reports — or only ever fails — locks every PR forever). A PR head is
#       also consulted because PR-only checks (e.g. "Dependency review", gated on
#       the pull_request event) never run on a push and so never appear on main's
#       HEAD. For determinism, pin the PR that vouches for the PR-only gates with
#       PR_CONTEXT_SOURCE=<pr> (the #216-after-retarget PR) rather than relying on
#       an arbitrary recent-PR window.
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
# BOOTSTRAP ORDER (do this before STEP=gates can pass its preflight):
#   1. Merge #215 (Supply-chain gate) and #216 (SemVer/Doc/Dependency-review/
#      Coverage gates) to main. The gate PRs are themselves blocked by the 1-
#      approval rule, so this first batch is Ocean's HC-7 merge.
#   2. #216 must target main (or an integration/** branch) for its CI to run at
#      all: ci.yml triggers on `pull_request: branches: [main, integration/**]`,
#      so while #216 is stacked on fix/issue-208-cve-gating its four new jobs
#      never fire (its head shows only "Pipeline Regression Gate"). Retarget it to
#      main before merge so the new gates actually run.
#   3. "Dependency review" is pull_request-only — it never appears on a push to
#      main. After the batch merges, a main-targeted PR must report it once; pin
#      that PR with PR_CONTEXT_SOURCE=<pr> when running STEP=gates so the preflight
#      sources it deterministically instead of from an arbitrary recent-PR window.
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

# Emit "name<TAB>status<TAB>conclusion" for every check-run on a commit-ish.
# conclusion is "" while a run is still in flight (jq `// ""`).
fetch_check_runs() {
  local ref="$1"
  gh api --paginate "repos/${REPO}/commits/${ref}/check-runs" \
    --jq '.check_runs[] | [.name, .status, (.conclusion // "")] | @tsv'
}

# A required context is "healthy" if ANY recent run concluded with one of these
# non-failing terminal conclusions — proof the gate is wired and CAN go green.
# This deliberately does not demand that every recent run passed: one unlucky PR
# that fails the check must not block activation, but a context that has only ever
# failed / is still running / never reported must (it would lock every PR once
# required).
ACCEPTABLE_CONCLUSIONS="success skipped neutral"

# Abort unless every required context has a recent PASSING run, sourced from:
#   1. main's HEAD              (MANDATORY — fetch failure aborts verification)
#   2. recent PR heads          (listing MANDATORY; each per-head fetch best-effort)
#   3. PR_CONTEXT_SOURCE=<pr>    (OPTIONAL but MANDATORY when set — pins a specific
#                                 PR so PR-only gates like "Dependency review" are
#                                 sourced deterministically, not from an arbitrary
#                                 last-N window. Set this to the #216-after-retarget
#                                 PR before activation.)
# PR heads are consulted because PR-only checks (e.g. "Dependency review", gated
# on `if: github.event_name == 'pull_request'`) never run on a push and so never
# appear on main's HEAD. Mandatory sources hard-fail rather than silently building
# a partial picture that could mis-report a context as "not reporting".
preflight_contexts_report() {
  # Probe API reachability first so a total outage reports as a verification
  # failure, not as "merge the gate PRs".
  if ! gh api "repos/${REPO}" --jq '.full_name' >/dev/null 2>&1; then
    echo "ABORT: cannot reach the GitHub API for ${REPO} (auth/network/rate-limit?)." >&2
    echo "Verification could not run; resolve API access and retry." >&2
    exit 1
  fi

  local runs
  # MANDATORY source 1: main HEAD.
  if ! runs="$(fetch_check_runs "${BRANCH}")"; then
    echo "ABORT: could not fetch check-runs for ${BRANCH} HEAD." >&2
    echo "Verification data incomplete; resolve API access and retry." >&2
    exit 1
  fi

  # MANDATORY source 2: list recent PR heads. The per-head fetches below are
  # best-effort, but if we cannot even enumerate PRs we cannot vouch for the
  # PR-only contexts, so abort rather than degrade to a partial union.
  local pr_shas
  if ! pr_shas="$(gh pr list --state all --limit 10 --json headRefOid \
                    --jq '.[].headRefOid')"; then
    echo "ABORT: could not list recent PRs to source PR-only check contexts." >&2
    echo "Verification data incomplete; resolve API access and retry." >&2
    exit 1
  fi

  # OPTIONAL deterministic source: a pinned PR whose head MUST be reachable.
  if [[ -n "${PR_CONTEXT_SOURCE:-}" ]]; then
    local src_sha src_runs
    if ! src_sha="$(gh pr view "${PR_CONTEXT_SOURCE}" --json headRefOid \
                      --jq '.headRefOid')"; then
      echo "ABORT: PR_CONTEXT_SOURCE=#${PR_CONTEXT_SOURCE} not found or unreachable." >&2
      exit 1
    fi
    if ! src_runs="$(fetch_check_runs "${src_sha}")"; then
      echo "ABORT: could not fetch check-runs for PR #${PR_CONTEXT_SOURCE} head ${src_sha}." >&2
      exit 1
    fi
    runs+=$'\n'"${src_runs}"
  fi

  # Best-effort: fold in each recent PR head. A dropped fetch here is tolerated
  # because the mandatory sources above carry the load.
  local pr_sha pr_runs
  while IFS= read -r pr_sha; do
    [[ -n "${pr_sha}" ]] || continue
    pr_runs="$(fetch_check_runs "${pr_sha}" 2>/dev/null)" || continue
    runs+=$'\n'"${pr_runs}"
  done <<<"${pr_shas}"

  # Evaluate health per required context.
  local ctx missing=() unhealthy=()
  for ctx in "${REQUIRED_CONTEXTS[@]}"; do
    local pairs healthy pair concl
    # status:conclusion for every run of this context across all sources.
    pairs="$(awk -F'\t' -v c="${ctx}" \
      '$1==c {print $2":"(($3=="")?"pending":$3)}' <<<"${runs}")"
    if [[ -z "${pairs}" ]]; then
      missing+=("${ctx}")
      continue
    fi
    healthy=false
    while IFS= read -r pair; do
      [[ -n "${pair}" ]] || continue
      concl="${pair##*:}"
      case " ${ACCEPTABLE_CONCLUSIONS} " in
        *" ${concl} "*) healthy=true; break ;;
      esac
    done <<<"${pairs}"
    if [[ "${healthy}" != "true" ]]; then
      unhealthy+=("${ctx} [$(tr '\n' ',' <<<"${pairs}" | sed 's/,$//')]")
    fi
  done

  if (( ${#missing[@]} > 0 )); then
    echo "ABORT: these required contexts have not reported on ${BRANCH} HEAD or any recent PR:" >&2
    printf '  - %s\n' "${missing[@]}" >&2
    echo "Merge the PR(s) that add them first (gate wall: #215 then #216), and make sure" >&2
    echo "they run against a main-targeted PR (set PR_CONTEXT_SOURCE=<pr> to pin one)." >&2
    echo "Requiring a check that never reports blocks every PR." >&2
    exit 1
  fi
  if (( ${#unhealthy[@]} > 0 )); then
    echo "ABORT: these required contexts report but have no recent passing run:" >&2
    printf '  - %s\n' "${unhealthy[@]}" >&2
    echo "A context marked required must be able to go green. Fix the failing gate, or" >&2
    echo "wait for a passing run (set PR_CONTEXT_SOURCE=<pr> to a PR where it passed)." >&2
    exit 1
  fi
  echo "Preflight OK: all ${#REQUIRED_CONTEXTS[@]} required contexts have a recent passing run."
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
  # Don't clobber an already-hardened environment. The env 404s before first
  # activation; if it already exists (a later rerun, a different admin, or manual
  # hardening), refuse to overwrite its reviewers/branch-policy unless FORCE=true.
  if gh api "repos/${REPO}/environments/${ENVIRONMENT}" >/dev/null 2>&1; then
    if [[ "${FORCE:-false}" != "true" ]]; then
      echo "ABORT: '${ENVIRONMENT}' environment already exists." >&2
      echo "Re-applying would replace its reviewers and branch policy with this" >&2
      echo "script's defaults. Set FORCE=true to overwrite intentionally." >&2
      exit 1
    fi
    echo "[release-gate] FORCE=true — overwriting the existing '${ENVIRONMENT}' environment."
  fi
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
