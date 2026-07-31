#!/usr/bin/env bash
# Append a GitHub Traffic API snapshot to the repository's traffic ledger.
#
# GitHub retains clone/view traffic for only 14 days. This script merges the
# current 14-day window into data/traffic-ledger.json so history accumulates
# instead of expiring. It is invoked by .github/workflows/traffic-tracker.yml
# every 13 days (one-day safety overlap); windows overlap by design and the
# merge takes the per-day maximum, since a partial day's count only grows
# toward its final value.
#
# Requires: gh (authenticated via GH_TOKEN), jq. The traffic API needs push
# access, which is why this runs in-repo with GITHUB_TOKEN rather than
# through any third-party tracker.
set -euo pipefail

REPO="${TRAFFIC_REPO:?TRAFFIC_REPO must be set, e.g. owner/name}"
LEDGER="${TRAFFIC_LEDGER:-data/traffic-ledger.json}"

clones_json=$(gh api "repos/${REPO}/traffic/clones")
views_json=$(gh api "repos/${REPO}/traffic/views")

# Fail closed: a response missing its payload key is a failed read, not an
# empty result. (An empty day-array with the key present is valid data.)
echo "$clones_json" | jq -e 'has("clones")' >/dev/null || {
    echo "ERROR: clones response lacks .clones key — treating as failed read" >&2
    exit 1
}
echo "$views_json" | jq -e 'has("views")' >/dev/null || {
    echo "ERROR: views response lacks .views key — treating as failed read" >&2
    exit 1
}

mkdir -p "$(dirname "$LEDGER")"
[ -f "$LEDGER" ] || echo '{"days":{}}' > "$LEDGER"
jq -e 'has("days")' "$LEDGER" >/dev/null || {
    echo "ERROR: existing ledger $LEDGER is malformed (no .days key); refusing to overwrite" >&2
    exit 1
}

merged=$(jq -n \
    --slurpfile ledger "$LEDGER" \
    --argjson clones "$clones_json" \
    --argjson views "$views_json" \
    --arg taken_at "$(date -u +%Y-%m-%dT%H:%M:%SZ)" '
    def day_map(rows; ck; uk):
        reduce rows[] as $r ({}; .[$r.timestamp[:10]] = {(ck): $r.count, (uk): $r.uniques});
    ($ledger[0].days // {}) as $old
    | day_map($clones.clones; "clones"; "unique_clones") as $c
    | day_map($views.views; "views"; "unique_views") as $v
    | (($old | keys) + ($c | keys) + ($v | keys) | unique) as $alldays
    | {
        days: (reduce $alldays[] as $d ({};
            .[$d] = {
                clones:         ([$old[$d].clones         // 0, $c[$d].clones         // 0] | max),
                unique_clones:  ([$old[$d].unique_clones  // 0, $c[$d].unique_clones  // 0] | max),
                views:          ([$old[$d].views          // 0, $v[$d].views          // 0] | max),
                unique_views:   ([$old[$d].unique_views   // 0, $v[$d].unique_views   // 0] | max)
            })),
        last_snapshot: {
            taken_at: $taken_at,
            clones_14d: $clones.count,
            unique_clones_14d: $clones.uniques,
            views_14d: $views.count,
            unique_views_14d: $views.uniques
        }
    }')

echo "$merged" | jq . > "$LEDGER"

day_total=$(jq '.days | length' "$LEDGER")
echo "ledger: $LEDGER — $day_total day rows; snapshot: $(echo "$merged" | jq -c .last_snapshot)"
