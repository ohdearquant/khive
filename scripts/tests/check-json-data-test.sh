#!/usr/bin/env bash
# Fixture test for scripts/check-json-data.sh.
# Builds a scratch repository per case; nothing is committed and no hook runs. Run with:
#   bash scripts/tests/check-json-data-test.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
GUARD="${CJD_SCRIPT:-$SCRIPT_DIR/check-json-data.sh}"
[ -f "$GUARD" ] || { echo "guard not found at $GUARD" >&2; exit 2; }

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

fresh_repo() {
  local d="$WORK/$1"
  rm -rf "$d"
  mkdir -p "$d"
  git -C "$d" init -q
  printf '%s\n' "$d"
}

# run_guard <repo> [--all] -> prints rc, captures stderr in $ERR
ERR="$WORK/stderr"
run_guard() {
  local repo="$1"; shift
  local rc=0
  (cd "$repo" && KHIVE_ALLOW_DATA=0 bash "$GUARD" "$@") 2>"$ERR" || rc=$?
  printf '%s' "$rc"
}

pass() { echo "PASS"; }
fail() { echo "FAIL: $1" >&2; cat "$ERR" >&2 || true; exit 1; }

echo "--- case 1: staged JSONL present in the working tree is refused (control) ---"
R=$(fresh_repo c1)
printf '{"a":1}\n' >"$R/data.jsonl"; git -C "$R" add data.jsonl
[ "$(run_guard "$R")" = 1 ] && grep -q 'data.jsonl' "$ERR" && pass || fail "staged data.jsonl was not refused"

echo "--- case 2: staged JSONL whose working-tree file was removed is still refused ---"
R=$(fresh_repo c2)
printf '{"a":1}\n' >"$R/data.jsonl"; git -C "$R" add data.jsonl; rm "$R/data.jsonl"
[ "$(run_guard "$R")" = 1 ] && grep -q 'data.jsonl' "$ERR" && pass || fail "staged data.jsonl with no working-tree file passed"

echo "--- case 3: staged oversize JSON is measured from the index, not the working tree ---"
R=$(fresh_repo c3)
head -c 307200 /dev/zero | tr '\0' 'x' >"$R/big.json"; git -C "$R" add big.json
printf 'x' >"$R/big.json"
[ "$(run_guard "$R")" = 1 ] && grep -q '300KB' "$ERR" && pass || fail "staged 300KB big.json passed after the working-tree copy was truncated"

echo "--- case 4: staged small JSON commits (must-pass control) ---"
R=$(fresh_repo c4)
printf '{"ok":true}\n' >"$R/config.json"; git -C "$R" add config.json
[ "$(run_guard "$R")" = 0 ] && pass || fail "small config.json was refused"

echo "--- case 5: staged JSONL with no working-tree file, deliberate bypass still works ---"
R=$(fresh_repo c5)
printf '{"a":1}\n' >"$R/data.jsonl"; git -C "$R" add data.jsonl; rm "$R/data.jsonl"
rc=0; (cd "$R" && KHIVE_ALLOW_DATA=1 bash "$GUARD") 2>"$ERR" || rc=$?
[ "$rc" = 0 ] && pass || fail "KHIVE_ALLOW_DATA=1 did not bypass"

echo "--- case 6: --all scans the working tree (untracked JSONL refused; index not consulted) ---"
R=$(fresh_repo c6)
printf '{"a":1}\n' >"$R/loose.jsonl"
[ "$(run_guard "$R" --all)" = 1 ] && grep -q 'loose.jsonl' "$ERR" && pass || fail "--all did not refuse an untracked loose.jsonl"

echo "--- case 7: benchmark-path JSONL staged with no working-tree file still passes ---"
R=$(fresh_repo c7)
mkdir -p "$R/bench"; printf '{"t":1}\n' >"$R/bench/run.jsonl"; git -C "$R" add bench/run.jsonl; rm "$R/bench/run.jsonl"
[ "$(run_guard "$R")" = 0 ] && pass || fail "bench/run.jsonl was refused"

# --- consumer-repo exemption file ---------------------------------------------
# Every case below builds the SAME 300KB tracked JSON. Cases 1-7 above are the
# control for "no exemption file present": that file is refused there, so a pass
# here is attributable to the exemption and not to a ceiling change.

big_json() {  # big_json <repo> <path> <kb>
  mkdir -p "$(dirname "$1/$2")"
  head -c $(( $3 * 1024 )) /dev/zero | tr '\0' 'x' >"$1/$2"
}

echo "--- case 8: a listed over-ceiling JSON passes in BOTH modes ---"
R=$(fresh_repo c8)
big_json "$R" data/manifest.json 300
printf '%s\n' '^data/manifest\.json$ 2048' >"$R/.check-json-data-exemptions"
git -C "$R" add data/manifest.json .check-json-data-exemptions
staged=$(run_guard "$R"); tree=$(run_guard "$R" --all)
[ "$staged" = 0 ] && [ "$tree" = 0 ] && pass || fail "listed manifest refused (staged=$staged --all=$tree)"

echo "--- case 9: the same file over its OWN declared ceiling is blocked ---"
R=$(fresh_repo c9)
big_json "$R" data/manifest.json 300
printf '%s\n' '^data/manifest\.json$ 100' >"$R/.check-json-data-exemptions"
git -C "$R" add data/manifest.json .check-json-data-exemptions
[ "$(run_guard "$R" --all)" = 1 ] && grep -q '100KB ceiling declared' "$ERR" && pass || fail "a file over its own declared ceiling passed"

echo "--- case 10: an unlisted over-ceiling JSON is still blocked beside a listed one ---"
R=$(fresh_repo c10)
big_json "$R" data/manifest.json 300
big_json "$R" data/other.json 300
printf '%s\n' '^data/manifest\.json$ 2048' >"$R/.check-json-data-exemptions"
git -C "$R" add data/manifest.json data/other.json .check-json-data-exemptions
[ "$(run_guard "$R" --all)" = 1 ] && grep -q 'data/other.json' "$ERR" && ! grep -q 'data/manifest.json' "$ERR" && pass || fail "the exemption did not stay scoped to the path it names"

echo "--- case 11: a malformed line fails CLOSED with rc=2, naming file and line ---"
R=$(fresh_repo c11)
big_json "$R" data/manifest.json 300
printf '%s\n' '# fine' '^data/manifest\.json$ not-a-number' >"$R/.check-json-data-exemptions"
git -C "$R" add data/manifest.json .check-json-data-exemptions
[ "$(run_guard "$R" --all)" = 2 ] && grep -q '.check-json-data-exemptions:2' "$ERR" && pass || fail "a malformed ceiling did not exit 2"

echo "--- case 12: an unanchored pattern is a configuration error, not a wide exemption ---"
R=$(fresh_repo c12)
big_json "$R" data/manifest.json 300
printf '%s\n' 'manifest\.json 2048' >"$R/.check-json-data-exemptions"
git -C "$R" add data/manifest.json .check-json-data-exemptions
[ "$(run_guard "$R" --all)" = 2 ] && grep -q 'must start with' "$ERR" && pass || fail "an unanchored pattern was accepted"

# The JSONL cases reuse case 1 and case 6 as their control: with no exemption
# file a staged or untracked JSONL file is refused, so a pass below is
# attributable to the line that names the file.
echo "--- case 13: an exemption line admits the JSONL file it names, staged and --all ---"
R=$(fresh_repo c13)
mkdir -p "$R/atoms"; printf '{"a":1}\n' >"$R/atoms/LEDGER.jsonl"
printf '%s\n' '^atoms/LEDGER\.jsonl$ 128' >"$R/.check-json-data-exemptions"
git -C "$R" add atoms/LEDGER.jsonl .check-json-data-exemptions
[ "$(run_guard "$R")" = 0 ] || fail "a staged JSONL file named by an exemption line was refused"
[ "$(run_guard "$R" --all)" = 0 ] && pass || fail "--all refused a JSONL file named by an exemption line"

echo "--- case 13b: an exempt JSONL file over its ceiling is refused, measured from the index ---"
R=$(fresh_repo c13b)
big_json "$R" atoms/LEDGER.jsonl 300
printf '%s\n' '^atoms/LEDGER\.jsonl$ 128' >"$R/.check-json-data-exemptions"
git -C "$R" add atoms/LEDGER.jsonl .check-json-data-exemptions
printf 'x' >"$R/atoms/LEDGER.jsonl"
[ "$(run_guard "$R")" = 1 ] && grep -q '300KB exceeds the 128KB ceiling' "$ERR" && pass || fail "an exempt JSONL file over its ceiling passed"

echo "--- case 13c: an unlisted JSONL file beside an exempt one is still refused ---"
R=$(fresh_repo c13c)
mkdir -p "$R/atoms"; printf '{"a":1}\n' >"$R/atoms/LEDGER.jsonl"; printf '{"a":1}\n' >"$R/atoms/export.jsonl"
printf '%s\n' '^atoms/LEDGER\.jsonl$ 128' >"$R/.check-json-data-exemptions"
git -C "$R" add atoms/LEDGER.jsonl atoms/export.jsonl .check-json-data-exemptions
[ "$(run_guard "$R")" = 1 ] && grep -q 'atoms/export.jsonl' "$ERR" && ! grep -q 'atoms/LEDGER.jsonl' "$ERR" && pass || fail "the JSONL exemption did not stay scoped to the path it names"

echo "--- case 13d: the JSONL exemption matches the path as tracked, not case-folded ---"
R=$(fresh_repo c13d)
mkdir -p "$R/atoms"; printf '{"a":1}\n' >"$R/atoms/LEDGER.jsonl"
printf '%s\n' '^atoms/ledger\.jsonl$ 128' >"$R/.check-json-data-exemptions"
git -C "$R" add atoms/LEDGER.jsonl .check-json-data-exemptions
[ "$(run_guard "$R")" = 1 ] && grep -qF 'atoms/LEDGER.jsonl — JSONL/NDJSON staged outside a benchmark-results path' "$ERR" && pass || fail "a path was covered by an exemption written for its lower-cased spelling"

echo "--- case 14: comments and blank lines are ignored ---"
R=$(fresh_repo c14)
big_json "$R" data/manifest.json 300
printf '%s\n' '# a comment' '' '   ' '^data/manifest\.json$ 2048' >"$R/.check-json-data-exemptions"
git -C "$R" add data/manifest.json .check-json-data-exemptions
[ "$(run_guard "$R" --all)" = 0 ] && pass || fail "comments or blank lines were treated as rules"

# --- names the extension rules must still see -----------------------------------
# Case 1 and case 6 are the controls: the lower-case ASCII spelling is refused. Each
# case below spells the same kind of file differently and must be refused the same way.

echo "--- case 15: an upper-case .JSONL extension is still JSONL ---"
R=$(fresh_repo c15)
printf '{"a":1}\n' >"$R/DATA.JSONL"; git -C "$R" add DATA.JSONL
[ "$(run_guard "$R")" = 1 ] && grep -qF 'DATA.JSONL — JSONL/NDJSON staged' "$ERR" && pass || fail "staged DATA.JSONL was not refused"

echo "--- case 16: a non-ASCII JSONL name is judged as named, not in quoted form ---"
R=$(fresh_repo c16)
mkdir -p "$R/notes"; printf '{"a":1}\n' >"$R/notes/données.jsonl"; git -C "$R" add notes/données.jsonl
[ "$(run_guard "$R")" = 1 ] && grep -qF 'notes/données.jsonl — JSONL/NDJSON staged' "$ERR" && pass || fail "staged notes/données.jsonl was not refused"

echo "--- case 17: --all refuses a mixed-case .NDJSON and a non-ASCII .jsonl ---"
R=$(fresh_repo c17)
mkdir -p "$R/exports"; printf '{"a":1}\n' >"$R/exports/Corpus.NDJSON"; printf '{"a":1}\n' >"$R/exports/résumé.jsonl"
[ "$(run_guard "$R" --all)" = 1 ] && grep -qF 'exports/Corpus.NDJSON' "$ERR" && grep -qF 'exports/résumé.jsonl' "$ERR" && pass || fail "--all did not refuse both spellings"

echo "--- case 18: an oversize upper-case .JSON is measured like .json ---"
R=$(fresh_repo c18)
big_json "$R" BIG.JSON 300; git -C "$R" add BIG.JSON
[ "$(run_guard "$R")" = 1 ] && grep -qF 'BIG.JSON — 300KB JSON exceeds' "$ERR" && pass || fail "staged 300KB BIG.JSON passed"

echo "--- case 19: an oversize non-ASCII .json is read from the index by its real name ---"
R=$(fresh_repo c19)
big_json "$R" data/größe.json 300; git -C "$R" add data/größe.json
[ "$(run_guard "$R")" = 1 ] && grep -qF 'data/größe.json — 300KB JSON exceeds' "$ERR" && pass || fail "staged 300KB data/größe.json passed"

echo "--- case 20: a small non-ASCII .json commits (must-pass control for the index read) ---"
R=$(fresh_repo c20)
mkdir -p "$R/data"; printf '{"ok":true}\n' >"$R/data/réglages.json"; git -C "$R" add data/réglages.json
[ "$(run_guard "$R")" = 0 ] && pass || fail "small data/réglages.json was refused"

echo "--- case 21: a staged name starting with '-' is still judged ---"
R=$(fresh_repo c21)
printf '{"a":1}\n' >"$R/-data.jsonl"; git -C "$R" add -- -data.jsonl
[ "$(run_guard "$R")" = 1 ] && grep -qF -- '-data.jsonl — JSONL/NDJSON staged' "$ERR" && pass || fail "staged -data.jsonl was not refused"

echo "--- case 22: --all judges names under a '-' directory and a '-' oversize .json ---"
R=$(fresh_repo c22)
mkdir -p "$R/-dir"; printf '{"a":1}\n' >"$R/-dir/x.NDJSON"; big_json "$R" -big.json 300
[ "$(run_guard "$R" --all)" = 1 ] && grep -qF -- '-dir/x.NDJSON' "$ERR" && grep -qF -- '-big.json — 300KB JSON exceeds' "$ERR" && pass || fail "--all did not refuse both '-' names"

echo "--- case 23: a JSONL path with 'result' before 'bench' is not a benchmark-results path ---"
R=$(fresh_repo c23)
mkdir -p "$R/results"; printf '{"a":1}\n' >"$R/results/bench-corpus.jsonl"; git -C "$R" add results/bench-corpus.jsonl
[ "$(run_guard "$R")" = 1 ] && grep -qF 'results/bench-corpus.jsonl — JSONL/NDJSON staged outside a benchmark-results path' "$ERR" && pass || fail "results/bench-corpus.jsonl was admitted"

echo "--- case 24: a *bench*result* JSONL outside a bench directory is admitted (control) ---"
R=$(fresh_repo c24)
mkdir -p "$R/perf"; printf '{"t":1}\n' >"$R/perf/bench_results.jsonl"; git -C "$R" add perf/bench_results.jsonl
[ "$(run_guard "$R")" = 0 ] && pass || fail "perf/bench_results.jsonl was refused"

echo "--- case 25: a nested package's lockfile over the ceiling is admitted by file name ---"
R=$(fresh_repo c25)
big_json "$R" apps/web/package-lock.json 300; git -C "$R" add apps/web/package-lock.json
[ "$(run_guard "$R")" = 0 ] && pass || fail "apps/web/package-lock.json was refused"

echo "all cases passed"
