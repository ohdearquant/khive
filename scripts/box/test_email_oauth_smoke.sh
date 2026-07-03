#!/usr/bin/env bash
# Fixture harness for the "Email channel auth smoke" helpers and stage in
# scripts/box/bootstrap.sh (the off-Studio box bootstrap kit's OAuth
# client-secret validity check). Two testing strategies, both against the
# code that actually ships, never a reimplementation:
#
#   1. The small helper functions (oauth_var, mask_value, is_guid_shaped,
#      scrub_value, extract_json_field) are copy-pasted verbatim below and
#      exercised directly.
#   2. The stage's control flow itself (the xtrace guard, the GUID-shape
#      gate, the set +e/set -e bracket, the single exit seam) is extracted
#      fresh from bootstrap.sh at run time via sed, then sourced inside an
#      isolated subshell (fake HOME, curl replaced by a network-free stub)
#      so this always tests the current shipped stage, never a stale copy.
#
# No network calls are made anywhere in this file. Every id, GUID, and
# secret in the fixtures below is fabricated for this test; none of them
# are, or have ever been, valid Microsoft Entra credentials.
#
# Run from anywhere; all state lives under a throwaway mktemp directory
# that is removed on exit.
set -u

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BOOTSTRAP_SH="$SCRIPT_DIR/bootstrap.sh"

SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/khive-box-email-smoke-test.XXXXXX")"
trap 'rm -rf "$SCRATCH"' EXIT
FIXDIR="$SCRATCH/fixtures"
mkdir -p "$FIXDIR"

PASS=0
FAIL=0

check() {
  local desc="$1" expect="$2" got="$3"
  if [ "$got" = "$expect" ]; then
    PASS=$((PASS + 1))
    echo "PASS: $desc"
  else
    FAIL=$((FAIL + 1))
    echo "FAIL: $desc"
    echo "      expected: $expect"
    echo "      got:      $got"
  fi
}

check_contains() {
  local desc="$1" needle="$2" haystack="$3"
  case "$haystack" in
    *"$needle"*)
      PASS=$((PASS + 1))
      echo "PASS: $desc"
      ;;
    *)
      FAIL=$((FAIL + 1))
      echo "FAIL: $desc"
      echo "      expected to contain: $needle"
      echo "      got: $haystack"
      ;;
  esac
}

check_not_contains() {
  local desc="$1" needle="$2" haystack="$3"
  case "$haystack" in
    *"$needle"*)
      FAIL=$((FAIL + 1))
      echo "FAIL: $desc"
      echo "      expected NOT to contain: $needle"
      echo "      got: $haystack"
      ;;
    *)
      PASS=$((PASS + 1))
      echo "PASS: $desc"
      ;;
  esac
}

check_bool() {
  local desc="$1" expect="$2" got
  shift 2
  if "$@" >/dev/null 2>&1; then got="true"; else got="false"; fi
  check "$desc" "$expect" "$got"
}

# --- Fixtures: sample ~/.khive/.env files ------------------------------------

cat > "$FIXDIR/env_all_quoted.env" <<'EOF'
KHIVE_EMAIL_OAUTH_CLIENT_ID="cid-quoted-111111"
KHIVE_EMAIL_OAUTH_CLIENT_SECRET="cs-quoted-222222"
KHIVE_EMAIL_OAUTH_TENANT_ID="tid-quoted-333333"
EOF

cat > "$FIXDIR/env_all_unquoted.env" <<'EOF'
KHIVE_EMAIL_OAUTH_CLIENT_ID=cid-unquoted-111111
KHIVE_EMAIL_OAUTH_CLIENT_SECRET=cs-unquoted-222222
KHIVE_EMAIL_OAUTH_TENANT_ID=tid-unquoted-333333
EOF

cat > "$FIXDIR/env_partial_one.env" <<'EOF'
KHIVE_EMAIL_OAUTH_CLIENT_ID=only-client-id-set
EOF

cat > "$FIXDIR/env_partial_two.env" <<'EOF'
KHIVE_EMAIL_OAUTH_CLIENT_ID=cid-partial
KHIVE_EMAIL_OAUTH_TENANT_ID=tid-partial
EOF

cat > "$FIXDIR/env_empty_value.env" <<'EOF'
KHIVE_EMAIL_OAUTH_CLIENT_ID=
KHIVE_EMAIL_OAUTH_CLIENT_SECRET=cs-set
KHIVE_EMAIL_OAUTH_TENANT_ID=tid-set
EOF

cat > "$FIXDIR/env_unrelated_only.env" <<'EOF'
KHIVE_EMAIL_USERNAME=someone@example.com
KHIVE_EMAIL_SMTP_HOST=smtp.example.com
EOF

# Fake-but-GUID-shaped triple: passes the is_guid_shaped gate, so the stage
# proceeds to the (stubbed) curl call in the stage-integration tests below.
FAKE_SMOKE_SECRET="xtrace-canary-secret-do-not-leak-9f8e7d"
cat > "$FIXDIR/env_all_three_fake.env" <<EOF
KHIVE_EMAIL_OAUTH_CLIENT_ID=66666666-7777-8888-9999-aaaaaaaaaaaa
KHIVE_EMAIL_OAUTH_CLIENT_SECRET=$FAKE_SMOKE_SECRET
KHIVE_EMAIL_OAUTH_TENANT_ID=11111111-2222-3333-4444-555555555555
EOF

# Malformed client_id (contains a glob metacharacter): all three vars are
# present (so the stage does not take the partial-config branch), but the
# id itself is not GUID-shaped.
cat > "$FIXDIR/env_malformed_client_id.env" <<'EOF'
KHIVE_EMAIL_OAUTH_CLIENT_ID=not-a-real-guid*marker-abc123
KHIVE_EMAIL_OAUTH_CLIENT_SECRET=some-fake-secret-value-for-testing
KHIVE_EMAIL_OAUTH_TENANT_ID=11111111-2222-3333-4444-555555555555
EOF

# Malformed tenant_id (contains embedded spaces): same shape, the other
# side of the gate.
cat > "$FIXDIR/env_malformed_tenant_id.env" <<'EOF'
KHIVE_EMAIL_OAUTH_CLIENT_ID=66666666-7777-8888-9999-aaaaaaaaaaaa
KHIVE_EMAIL_OAUTH_CLIENT_SECRET=some-fake-secret-value-for-testing
KHIVE_EMAIL_OAUTH_TENANT_ID=has spaces not a guid
EOF

# --- Verbatim copies of the functions under test -----------------------------
# (Copy-pasted from scripts/box/bootstrap.sh, not reimplemented, so this
# exercises the actual logic that ships. stage() is included because the
# extracted stage fragment used below calls it as its first line.)

stage() {
  printf '\n==> [%s] %s\n' "$(date -u +%H:%M:%S)" "$1"
}

oauth_var() {
  local name="$1" file="$2" existing raw val
  case "$name" in
    KHIVE_EMAIL_OAUTH_CLIENT_ID) existing="${KHIVE_EMAIL_OAUTH_CLIENT_ID:-}" ;;
    KHIVE_EMAIL_OAUTH_CLIENT_SECRET) existing="${KHIVE_EMAIL_OAUTH_CLIENT_SECRET:-}" ;;
    KHIVE_EMAIL_OAUTH_TENANT_ID) existing="${KHIVE_EMAIL_OAUTH_TENANT_ID:-}" ;;
    *) existing="" ;;
  esac
  if [ -n "$existing" ]; then
    printf '%s\n' "$existing"
    return 0
  fi
  [ -f "$file" ] || return 1
  raw="$(grep -E "^[[:space:]]*${name}[[:space:]]*=" "$file" 2>/dev/null | head -n 1)"
  [ -n "$raw" ] || return 1
  val="${raw#*=}"
  val="${val#"${val%%[![:space:]]*}"}"
  val="${val%"${val##*[![:space:]]}"}"
  case "$val" in
    \"*\") val="${val#\"}"; val="${val%\"}" ;;
    \'*\') val="${val#\'}"; val="${val%\'}" ;;
  esac
  [ -n "$val" ] || return 1
  printf '%s\n' "$val"
}

mask_value() {
  printf '%s...\n' "${1:0:6}"
}

is_guid_shaped() {
  local re='^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$'
  [[ "$1" =~ $re ]]
}

scrub_value() {
  local text="$1" value="$2" masked
  if [ -z "$value" ]; then
    printf '%s\n' "$text"
    return 0
  fi
  masked="$(mask_value "$value")"
  printf '%s\n' "${text//$value/$masked}"
}

extract_json_field() {
  local json="$1" field="$2"
  if command -v python3 >/dev/null 2>&1; then
    printf '%s' "$json" | python3 -c '
import json, sys
try:
    data = json.loads(sys.stdin.read())
except Exception:
    sys.exit(1)
value = data.get(sys.argv[1])
if not isinstance(value, str) or not value:
    sys.exit(1)
sys.stdout.write(value)
' "$field"
    return $?
  fi
  local match
  match="$(printf '%s' "$json" | grep -o "\"${field}\"[[:space:]]*:[[:space:]]*\"[^\"]*\"" | head -n 1)"
  [ -n "$match" ] || return 1
  match="${match#*:}"
  match="${match#"${match%%[![:space:]]*}"}"
  match="${match#\"}"
  match="${match%\"}"
  [ -n "$match" ] || return 1
  printf '%s\n' "$match"
}

# Test-only twin that always takes the grep/sed branch, regardless of
# whether python3 is on PATH, so the fallback body itself is exercised
# deterministically on every machine this harness runs on.
extract_json_field_fallback_only() {
  local json="$1" field="$2" match
  match="$(printf '%s' "$json" | grep -o "\"${field}\"[[:space:]]*:[[:space:]]*\"[^\"]*\"" | head -n 1)"
  [ -n "$match" ] || return 1
  match="${match#*:}"
  match="${match#"${match%%[![:space:]]*}"}"
  match="${match#\"}"
  match="${match%\"}"
  [ -n "$match" ] || return 1
  printf '%s\n' "$match"
}

# --- oauth_var: file parsing --------------------------------------------------

echo "=== oauth_var: file parsing ==="

v="$(oauth_var KHIVE_EMAIL_OAUTH_CLIENT_ID "$FIXDIR/env_all_quoted.env")"
check "quoted value: strips surrounding double quotes" "cid-quoted-111111" "$v"

v="$(oauth_var KHIVE_EMAIL_OAUTH_CLIENT_SECRET "$FIXDIR/env_all_quoted.env")"
check "quoted value (secret var): strips surrounding double quotes" "cs-quoted-222222" "$v"

v="$(oauth_var KHIVE_EMAIL_OAUTH_TENANT_ID "$FIXDIR/env_all_unquoted.env")"
check "unquoted value: read as-is" "tid-unquoted-333333" "$v"

v="$(oauth_var KHIVE_EMAIL_OAUTH_CLIENT_SECRET "$FIXDIR/env_partial_one.env" 2>/dev/null || echo "__MISSING__")"
check "var absent from file: oauth_var fails (empty result)" "__MISSING__" "$v"

v="$(oauth_var KHIVE_EMAIL_OAUTH_CLIENT_ID "$FIXDIR/env_empty_value.env" 2>/dev/null || echo "__MISSING__")"
check "var present but empty in file: treated as absent" "__MISSING__" "$v"

v="$(oauth_var KHIVE_EMAIL_OAUTH_CLIENT_ID "$FIXDIR/env_unrelated_only.env" 2>/dev/null || echo "__MISSING__")"
check "var not in file at all: oauth_var fails" "__MISSING__" "$v"

v="$(oauth_var KHIVE_EMAIL_OAUTH_CLIENT_ID "$SCRATCH/does-not-exist.env" 2>/dev/null || echo "__MISSING__")"
check "file does not exist: oauth_var fails without erroring" "__MISSING__" "$v"

# --- oauth_var: real env wins over file (documented precedence) -------------

echo ""
echo "=== oauth_var: process env takes precedence over the file ==="

KHIVE_EMAIL_OAUTH_CLIENT_ID="from-real-env"
export KHIVE_EMAIL_OAUTH_CLIENT_ID
v="$(oauth_var KHIVE_EMAIL_OAUTH_CLIENT_ID "$FIXDIR/env_all_quoted.env")"
check "already-exported var beats the file value" "from-real-env" "$v"
unset KHIVE_EMAIL_OAUTH_CLIENT_ID

v="$(oauth_var KHIVE_EMAIL_OAUTH_CLIENT_ID "$FIXDIR/env_all_quoted.env")"
check "after unset, file value is used again" "cid-quoted-111111" "$v"

# --- PRESENT_COUNT classification (mirrors the stage's own 0 / 1-2 / 3 split) --

echo ""
echo "=== EMAIL_OAUTH_PRESENT classification ==="

count_present() {
  local file="$1" n=0
  oauth_var KHIVE_EMAIL_OAUTH_CLIENT_ID "$file" >/dev/null 2>&1 && n=$((n + 1))
  oauth_var KHIVE_EMAIL_OAUTH_CLIENT_SECRET "$file" >/dev/null 2>&1 && n=$((n + 1))
  oauth_var KHIVE_EMAIL_OAUTH_TENANT_ID "$file" >/dev/null 2>&1 && n=$((n + 1))
  echo "$n"
}

v="$(count_present "$FIXDIR/env_all_quoted.env")"
check "all three set: PRESENT_COUNT == 3" "3" "$v"

v="$(count_present "$FIXDIR/env_partial_one.env")"
check "one set: PRESENT_COUNT == 1" "1" "$v"

v="$(count_present "$FIXDIR/env_partial_two.env")"
check "two set: PRESENT_COUNT == 2" "2" "$v"

v="$(count_present "$FIXDIR/env_unrelated_only.env")"
check "none set: PRESENT_COUNT == 0" "0" "$v"

# --- mask_value ---------------------------------------------------------------

echo ""
echo "=== mask_value ==="

v="$(mask_value "abcdefgh-secret")"
check "masks to first 6 chars + ellipsis" "abcdef..." "$v"

v="$(mask_value "ab")"
check "short value: no padding, just what's there + ellipsis" "ab..." "$v"

v="$(mask_value "")"
check "empty value: just the ellipsis" "..." "$v"

# --- is_guid_shaped ------------------------------------------------------------

echo ""
echo "=== is_guid_shaped ==="

check_bool "accepts a lowercase-hex GUID" "true" is_guid_shaped "11111111-2222-3333-4444-555555555555"
check_bool "accepts an uppercase-hex GUID" "true" is_guid_shaped "AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE"
check_bool "accepts a mixed-case-hex GUID" "true" is_guid_shaped "66666666-7777-8888-9999-aaaaaaaaaaaa"
check_bool "rejects a value containing a glob star" "false" is_guid_shaped "not-a-real-guid*marker-abc123"
check_bool "rejects a value containing an embedded space" "false" is_guid_shaped "11111111-2222-3333-4444-5555555555 5"
check_bool "rejects a value containing brackets" "false" is_guid_shaped "[1111111]-2222-3333-4444-555555555555"
check_bool "rejects a too-short value" "false" is_guid_shaped "11111111-2222-3333-4444-55555555"
check_bool "rejects a too-long value" "false" is_guid_shaped "11111111-2222-3333-4444-5555555555555555"
check_bool "rejects a value with non-hex characters" "false" is_guid_shaped "gggggggg-2222-3333-4444-555555555555"
check_bool "rejects an empty string" "false" is_guid_shaped ""
check_bool "rejects a plain word with no GUID structure" "false" is_guid_shaped "not-a-guid-at-all"

# --- extract_json_field + scrub_value against a real AADSTS response --------

echo ""
echo "=== extract_json_field + scrub_value against a real AADSTS90002 response ==="

# Captured from a live (rejected) token request against a nonexistent fake
# tenant. Contains no credentials; the tenant GUID and trace/correlation
# IDs are not secrets, and the tenant does not exist.
LIVE_JSON='{"error":"invalid_request","error_description":"AADSTS90002: Tenant '"'"'11111111-2222-3333-4444-555555555555'"'"' not found. Check to make sure you have the correct tenant ID and are signing into the correct cloud. Check with your subscription administrator, this may happen if there are no active subscriptions for the tenant. Trace ID: 06d036b0-8892-4cb4-94f9-8a2d3e9c2000 Correlation ID: f1b414c3-83bd-457f-bb87-a358fd5232e1 Timestamp: 2026-07-02 14:44:48Z","error_codes":[90002],"timestamp":"2026-07-02 14:44:48Z","trace_id":"06d036b0-8892-4cb4-94f9-8a2d3e9c2000","correlation_id":"f1b414c3-83bd-457f-bb87-a358fd5232e1","error_uri":"https://login.microsoftonline.com/error?code=90002"}'

FAKE_TENANT="11111111-2222-3333-4444-555555555555"
FAKE_CLIENT="66666666-7777-8888-9999-aaaaaaaaaaaa"

err="$(extract_json_field "$LIVE_JSON" "error")"
check "extract_json_field: error field" "invalid_request" "$err"

desc="$(extract_json_field "$LIVE_JSON" "error_description")"
check_contains "extract_json_field: error_description contains AADSTS code" "AADSTS90002" "$desc"
check_contains "extract_json_field: error_description preserves apostrophes around the tenant" "Tenant '11111111-2222-3333-4444-555555555555' not found" "$desc"
check_contains "extract_json_field: error_description runs to the real end (Timestamp line)" "Timestamp: 2026-07-02" "$desc"

scrubbed="$(scrub_value "$desc" "$FAKE_TENANT")"
check_not_contains "scrub_value: raw tenant GUID removed from error_description" "$FAKE_TENANT" "$scrubbed"
check_contains "scrub_value: masked tenant GUID present instead" "111111..." "$scrubbed"

scrubbed="$(scrub_value "$desc" "$FAKE_CLIENT")"
check "scrub_value: scrubbing a value that is not present is a no-op" "$desc" "$scrubbed"

err_fb="$(extract_json_field_fallback_only "$LIVE_JSON" "error")"
check "fallback path: error field" "invalid_request" "$err_fb"

desc_fb="$(extract_json_field_fallback_only "$LIVE_JSON" "error_description")"
check_contains "fallback path: error_description contains AADSTS code" "AADSTS90002" "$desc_fb"
check_contains "fallback path: error_description preserves apostrophes around the tenant" "Tenant '11111111-2222-3333-4444-555555555555' not found" "$desc_fb"
check "fallback path: does not confuse error_description with error (no substring bleed)" "invalid_request" "$err_fb"

scrubbed_fb="$(scrub_value "$desc_fb" "$FAKE_TENANT")"
check_not_contains "fallback path + scrub_value: raw tenant GUID removed" "$FAKE_TENANT" "$scrubbed_fb"

FAKE_SUCCESS='{"token_type":"Bearer","expires_in":3599,"ext_expires_in":3599,"access_token":"fake.not.a.real.jwt.eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"}'
has_token="$(printf '%s' "$FAKE_SUCCESS" | grep -q '"access_token"' && echo "yes" || echo "no")"
check "PASS-shaped response: access_token detection works" "yes" "$has_token"

# --- extract_json_field: real dispatcher without python3 on PATH ------------

echo ""
echo "=== extract_json_field: real dispatcher falls back correctly when python3 is not on PATH ==="

NOPY_DIR="$SCRATCH/no-python-path"
mkdir -p "$NOPY_DIR"
for bin in grep head; do
  src="$(command -v "$bin")"
  ln -sf "$src" "$NOPY_DIR/$bin"
done

err_nopy="$(
  (
    PATH="$NOPY_DIR"
    extract_json_field "$LIVE_JSON" "error"
  ) 2>&1
)"
check "extract_json_field (real dispatcher, not the _fallback_only twin): takes the grep/sed path and still extracts 'error' correctly" "invalid_request" "$err_nopy"

desc_nopy="$(
  (
    PATH="$NOPY_DIR"
    extract_json_field "$LIVE_JSON" "error_description"
  ) 2>&1
)"
check_contains "extract_json_field (no python3 on PATH): error_description still extracted" "AADSTS90002" "$desc_nopy"

# --- static-shape checks against the real bootstrap.sh -----------------------

echo ""
echo "=== static-shape checks against the shipped bootstrap.sh ==="

if grep -n 'scrub_value' "$BOOTSTRAP_SH" | grep -v 'EMAIL_CLIENT_SECRET' | grep -q .; then
  PASS=$((PASS + 1))
  echo "PASS: scrub_value is called in bootstrap.sh (sanity: the function is actually used)"
else
  FAIL=$((FAIL + 1))
  echo "FAIL: scrub_value does not appear to be called anywhere in bootstrap.sh"
fi

if grep -n 'scrub_value.*EMAIL_CLIENT_SECRET\|EMAIL_CLIENT_SECRET.*scrub_value' "$BOOTSTRAP_SH" | grep -q .; then
  FAIL=$((FAIL + 1))
  echo "FAIL: scrub_value appears to be called with EMAIL_CLIENT_SECRET (the secret must never reach scrub_value's text argument)"
else
  PASS=$((PASS + 1))
  echo "PASS: scrub_value is never called with EMAIL_CLIENT_SECRET"
fi

if grep -nE '(echo|printf).*EMAIL_CLIENT_SECRET' "$BOOTSTRAP_SH" | grep -q .; then
  FAIL=$((FAIL + 1))
  echo "FAIL: found an echo/printf that references EMAIL_CLIENT_SECRET directly in bootstrap.sh"
else
  PASS=$((PASS + 1))
  echo "PASS: no echo/printf in bootstrap.sh references EMAIL_CLIENT_SECRET directly"
fi

if grep -n 'is_guid_shaped "\$EMAIL_CLIENT_ID"' "$BOOTSTRAP_SH" | grep -q .; then
  PASS=$((PASS + 1))
  echo "PASS: bootstrap.sh validates EMAIL_CLIENT_ID with is_guid_shaped before the curl call"
else
  FAIL=$((FAIL + 1))
  echo "FAIL: no is_guid_shaped check found for EMAIL_CLIENT_ID in bootstrap.sh"
fi

if grep -n 'is_guid_shaped "\$EMAIL_TENANT_ID"' "$BOOTSTRAP_SH" | grep -q .; then
  PASS=$((PASS + 1))
  echo "PASS: bootstrap.sh validates EMAIL_TENANT_ID with is_guid_shaped before the curl call"
else
  FAIL=$((FAIL + 1))
  echo "FAIL: no is_guid_shaped check found for EMAIL_TENANT_ID in bootstrap.sh"
fi

if grep -n '__smoke_had_xtrace=1' "$BOOTSTRAP_SH" | grep -q .; then
  PASS=$((PASS + 1))
  echo "PASS: bootstrap.sh captures the caller's xtrace state before resolving any OAuth value"
else
  FAIL=$((FAIL + 1))
  echo "FAIL: no xtrace-capture guard found in bootstrap.sh"
fi

# --- sanity check: this harness's leak-detection method actually detects a leak

echo ""
echo "=== sanity check: xtrace DOES print an assigned value when left on unguarded ==="
echo "    (proves the 'fake secret never appears' assertions below are not vacuous)"

XTRACE_CANARY="xtrace-mechanism-canary-3f9c1a"
sanity_trace_out="$(bash -c 'set -x; PROBE_VALUE="'"$XTRACE_CANARY"'"; : "$PROBE_VALUE"' 2>&1)"
check_contains "sanity: plain bash xtrace prints an assigned value when tracing is left on" "$XTRACE_CANARY" "$sanity_trace_out"

# --- stage-level integration: extract the live stage from bootstrap.sh ------

echo ""
echo "=== email-auth-smoke stage: dynamic extraction from the shipped bootstrap.sh ==="

STAGE_FRAGMENT="$SCRATCH/extracted_email_smoke_stage.sh"
sed -n '/^stage "Email channel auth smoke"$/,/^set -e$/p' "$BOOTSTRAP_SH" > "$STAGE_FRAGMENT"
STAGE_FRAGMENT_LINES="$(wc -l < "$STAGE_FRAGMENT" | tr -d ' ')"
if [ "$STAGE_FRAGMENT_LINES" -lt 20 ]; then
  echo "FATAL: extracted email-auth-smoke stage from $BOOTSTRAP_SH is suspiciously" >&2
  echo "       short ($STAGE_FRAGMENT_LINES lines). The anchor lines this harness"   >&2
  echo "       sed's on ('stage \"Email channel auth smoke\"' .. 'set -e') may have" >&2
  echo "       changed in bootstrap.sh; update this harness to match."               >&2
  exit 1
fi

CURL_MARKER="$SCRATCH/.curl_called_marker"

# Sources the ACTUAL current email-auth-smoke stage (extracted fresh above,
# not reimplemented) inside an isolated subshell: a fake HOME so DOTENV
# resolves to a fixture file, the three KHIVE_EMAIL_OAUTH_* process-env vars
# unset so only the file is in play, and curl replaced by a network-free
# stub so no request ever leaves this machine. Sets globals STAGE_OUT
# (combined stdout+stderr) and STAGE_RC (exit status of the sourced stage).
run_stage_case() {
  local label="$1" fixture="$2" trace_mode="$3" fakehome
  fakehome="$SCRATCH/fakehome_$label"
  rm -rf "$fakehome"
  mkdir -p "$fakehome/.khive"
  [ -n "$fixture" ] && cp "$fixture" "$fakehome/.khive/.env"
  rm -f "$CURL_MARKER"
  STAGE_OUT="$(
    (
      set -e
      HOME="$fakehome"
      export HOME
      unset KHIVE_EMAIL_OAUTH_CLIENT_ID KHIVE_EMAIL_OAUTH_CLIENT_SECRET KHIVE_EMAIL_OAUTH_TENANT_ID
      curl() { : >"$CURL_MARKER"; printf '%s' "$LIVE_JSON"; }
      [ "$trace_mode" = "traced" ] && set -x
      # shellcheck source=/dev/null
      . "$STAGE_FRAGMENT"
    ) 2>&1
  )"
  STAGE_RC=$?
}

echo ""
echo "--- case: unconfigured (0 vars) ---"
run_stage_case "unconfigured" "" "plain"
check "unconfigured: exits 0 (non-fatal)" "0" "$STAGE_RC"
check_contains "unconfigured: prints the skip note" "email OAuth not configured; skipping auth smoke" "$STAGE_OUT"
[ -f "$CURL_MARKER" ] && curl_called="yes" || curl_called="no"
check "unconfigured: never calls curl" "no" "$curl_called"

echo ""
echo "--- case: partial config (2 of 3 vars) ---"
run_stage_case "partial" "$FIXDIR/env_partial_two.env" "plain"
check "partial config: exits 0 (non-fatal)" "0" "$STAGE_RC"
check_contains "partial config: prints the partial-config warning banner" "WARNING: partial email OAuth config" "$STAGE_OUT"
check_contains "partial config: names the missing var" "KHIVE_EMAIL_OAUTH_CLIENT_SECRET" "$STAGE_OUT"
[ -f "$CURL_MARKER" ] && curl_called="yes" || curl_called="no"
check "partial config: never calls curl" "no" "$curl_called"

echo ""
echo "--- case: malformed client_id (contains a glob star) ---"
run_stage_case "malformed_cid" "$FIXDIR/env_malformed_client_id.env" "plain"
check "malformed client_id: exits 0 (non-fatal)" "0" "$STAGE_RC"
check_contains "malformed client_id: warning names KHIVE_EMAIL_OAUTH_CLIENT_ID" "KHIVE_EMAIL_OAUTH_CLIENT_ID" "$STAGE_OUT"
check_contains "malformed client_id: warning says not GUID-shaped" "GUID-shaped" "$STAGE_OUT"
check_contains "malformed client_id: warning says the smoke is being skipped" "skipping email" "$STAGE_OUT"
check_contains "malformed client_id: masked value shown instead of raw" "not-a-..." "$STAGE_OUT"
check_not_contains "malformed client_id: raw value prefix never printed" "not-a-real-guid" "$STAGE_OUT"
check_not_contains "malformed client_id: raw value suffix never printed" "marker-abc123" "$STAGE_OUT"
[ -f "$CURL_MARKER" ] && curl_called="yes" || curl_called="no"
check "malformed client_id: never reaches curl (gated before the network call)" "no" "$curl_called"

echo ""
echo "--- case: malformed tenant_id (contains embedded spaces) ---"
run_stage_case "malformed_tid" "$FIXDIR/env_malformed_tenant_id.env" "plain"
check "malformed tenant_id: exits 0 (non-fatal)" "0" "$STAGE_RC"
check_contains "malformed tenant_id: warning names KHIVE_EMAIL_OAUTH_TENANT_ID" "KHIVE_EMAIL_OAUTH_TENANT_ID" "$STAGE_OUT"
check_contains "malformed tenant_id: warning says not GUID-shaped" "GUID-shaped" "$STAGE_OUT"
check_contains "malformed tenant_id: warning says the smoke is being skipped" "skipping email" "$STAGE_OUT"
check_contains "malformed tenant_id: masked value shown instead of raw" "has sp..." "$STAGE_OUT"
check_not_contains "malformed tenant_id: raw value never printed" "has spaces not a guid" "$STAGE_OUT"
[ -f "$CURL_MARKER" ] && curl_called="yes" || curl_called="no"
check "malformed tenant_id: never reaches curl (gated before the network call)" "no" "$curl_called"

echo ""
echo "--- case: all three present and GUID-shaped -> reaches the (stubbed) curl call ---"
run_stage_case "all_fake_valid" "$FIXDIR/env_all_three_fake.env" "plain"
check "all-fake-valid: exits 0 (non-fatal)" "0" "$STAGE_RC"
[ -f "$CURL_MARKER" ] && curl_called="yes" || curl_called="no"
check "all-fake-valid: DOES reach curl (the GUID gate lets a well-shaped fake pair through)" "yes" "$curl_called"
check_contains "all-fake-valid: FAIL banner prints (stub returns the AADSTS90002 fixture)" "WARNING: email channel auth smoke FAILED" "$STAGE_OUT"
check_contains "all-fake-valid: the underlying AADSTS error code still surfaces" "AADSTS90002" "$STAGE_OUT"
check_not_contains "all-fake-valid: raw tenant GUID scrubbed from the FAIL output" "11111111-2222-3333-4444-555555555555" "$STAGE_OUT"
check_not_contains "all-fake-valid: fake secret never appears, even untraced" "$FAKE_SMOKE_SECRET" "$STAGE_OUT"

echo ""
echo "--- case: same fixture, with shell tracing (set -x) active throughout (Finding 2 regression) ---"
run_stage_case "all_fake_valid_traced" "$FIXDIR/env_all_three_fake.env" "traced"
check "traced: exits 0 (non-fatal even under active xtrace)" "0" "$STAGE_RC"
[ -f "$CURL_MARKER" ] && curl_called="yes" || curl_called="no"
check "traced: still reaches the (stubbed) curl call (the risky code path was actually exercised)" "yes" "$curl_called"
check_not_contains "traced: fake secret NEVER appears in combined stdout+stderr, even under set -x" "$FAKE_SMOKE_SECRET" "$STAGE_OUT"

echo ""
echo "TOTAL: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
