#!/usr/bin/env bash
# scripts/perf/measure.sh — khive engine baseline measurement harness
#
# Produces reproducible measurements of:
#   - Release build wall time (full + incremental)
#   - Binary sizes
#   - Cold-start latency (spawn → first MCP response)
#   - Idle RSS: empty DB and seeded DB
#
# Usage:
#   cd <worktree-root>
#   bash scripts/perf/measure.sh [--skip-build] [--out-dir <dir>]
#
# Environment:
#   KHIVE_PERF_DIR   scratch dir for DBs (default: /tmp/khive-perf)
#   KHIVE_CRATES_DIR crates/ directory (default: ./crates)
#   OUT_DIR          output dir (default: /tmp/khive-perf)
#
# Requirements:
#   - Rust toolchain with cargo in PATH
#   - uv (for Python smoke tests)
#   - Python 3 (for smoke test timing)
#   - ps, date (BSD/macOS compatible)
#
# Notes:
#   - NEVER modifies ~/.khive — all DBs are written to KHIVE_PERF_DIR
#   - Incremental build uses `touch` on a stable leaf crate (khive-bm25)
#   - Cold-start probe: sends MCP initialize + tools/list then exits

set -euo pipefail

WORKTREE_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CRATES_DIR="${KHIVE_CRATES_DIR:-$WORKTREE_ROOT/crates}"
PERF_DIR="${KHIVE_PERF_DIR:-/tmp/khive-perf}"
OUT_DIR="${OUT_DIR:-$PERF_DIR}"
BINARY="$CRATES_DIR/target/release/kkernel"
SKIP_BUILD=0

for arg in "$@"; do
  case $arg in
    --skip-build) SKIP_BUILD=1 ;;
    --out-dir) OUT_DIR="${2:-$PERF_DIR}"; shift ;;
  esac
done

mkdir -p "$PERF_DIR" "$OUT_DIR"

LOG="$OUT_DIR/measure.log"
exec > >(tee -a "$LOG") 2>&1

echo "=== khive engine baseline measurement ==="
echo "Date:     $(date -Iseconds)"
echo "Worktree: $WORKTREE_ROOT"
echo "Crates:   $CRATES_DIR"
echo "PerfDir:  $PERF_DIR"
echo "OutDir:   $OUT_DIR"
echo ""

# ─── 1. RELEASE BUILD ────────────────────────────────────────────────────────

if [[ $SKIP_BUILD -eq 0 ]]; then
  echo "--- Full release build ---"
  BUILD_START=$(date +%s)
  (cd "$CRATES_DIR" && cargo build --workspace --release 2>&1)
  BUILD_END=$(date +%s)
  FULL_BUILD_SECS=$((BUILD_END - BUILD_START))
  echo "FULL_BUILD_WALL_SECONDS=$FULL_BUILD_SECS"
  echo ""

  echo "--- Incremental release build (touch khive-bm25/src/lib.rs) ---"
  LEAF_FILE="$CRATES_DIR/khive-bm25/src/lib.rs"
  if [[ -f "$LEAF_FILE" ]]; then
    touch "$LEAF_FILE"
    INC_START=$(date +%s)
    (cd "$CRATES_DIR" && cargo build --workspace --release 2>&1)
    INC_END=$(date +%s)
    INC_SECS=$((INC_END - INC_START))
    echo "INCREMENTAL_BUILD_WALL_SECONDS=$INC_SECS"
  else
    echo "INCREMENTAL_BUILD_WALL_SECONDS=skipped (leaf file not found)"
  fi
  echo ""
fi

# ─── 2. BINARY SIZES ─────────────────────────────────────────────────────────

echo "--- Binary sizes: crates/target/release/ ---"
ls -la "$CRATES_DIR/target/release/" 2>/dev/null | grep -v '^total' | grep -v '^d' | awk '{print $5, $NF}' | sort -rn | head -20
echo ""

echo "--- khive-mcp specific ---"
ls -la "$BINARY" 2>/dev/null || echo "khive-mcp NOT FOUND"
echo ""

# ─── 3. COLD-START LATENCY ───────────────────────────────────────────────────

cold_start_ms() {
  local db_path="$1"
  local extra_args="${2:-}"
  local start_ns end_ns

  # Python probe: spawn binary, send initialize, receive response, exit
  # Records wall time from spawn to first response line
  python3 - "$BINARY" "$db_path" $extra_args <<'PYEOF'
import sys, json, subprocess, time, os

binary = sys.argv[1]
db_path = sys.argv[2]
extra = sys.argv[3:]

env = {**os.environ, "KHIVE_NO_DAEMON": "1"}
args = [binary, "mcp", "--db", db_path, "--no-embed", "--log", "error"] + extra

start = time.perf_counter()
proc = subprocess.Popen(args, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                        stderr=subprocess.PIPE, env=env)

msg = json.dumps({"jsonrpc":"2.0","id":1,"method":"initialize",
                  "params":{"protocolVersion":"2024-11-05","capabilities":{},
                            "clientInfo":{"name":"perf-probe","version":"0.1"}}}) + "\n"
proc.stdin.write(msg.encode())
proc.stdin.flush()

line = proc.stdout.readline()
end = time.perf_counter()

proc.stdin.close()
proc.wait(timeout=5)

resp = json.loads(line)
assert resp.get("result", {}).get("serverInfo", {}).get("name") == "khive-mcp", f"unexpected: {resp}"
ms = (end - start) * 1000
print(f"COLD_START_MS={ms:.1f}")
PYEOF
}

echo "--- Cold-start latency (empty :memory: DB) ---"
cold_start_ms ":memory:"
echo ""

echo "--- Cold-start latency (empty file DB) ---"
EMPTY_DB="$PERF_DIR/empty.db"
rm -f "$EMPTY_DB"
cold_start_ms "$EMPTY_DB"
echo ""

# ─── 4. IDLE RSS ─────────────────────────────────────────────────────────────

idle_rss_kb() {
  local db_path="$1"
  local label="$2"
  local extra_args="${3:-}"

  local env_str="KHIVE_NO_DAEMON=1"
  eval "$env_str" "$BINARY" --db "$db_path" --no-embed --log error $extra_args &
  local pid=$!

  # Wait for process to be ready (send initialize, wait for response)
  sleep 0.5  # brief settle; binary is fast to start

  # Measure RSS (macOS ps returns KB)
  local rss
  rss=$(ps -o rss= -p "$pid" 2>/dev/null || echo "0")
  echo "IDLE_RSS_${label}_KB=$rss"

  kill "$pid" 2>/dev/null || true
  wait "$pid" 2>/dev/null || true
}

echo "--- Idle RSS: empty :memory: DB ---"
# Use Python to measure RSS after the server is ready (post-initialize)
python3 - "$BINARY" ":memory:" <<'PYEOF'
import sys, json, subprocess, time, os

binary = sys.argv[1]
db_path = sys.argv[2]

env = {**os.environ, "KHIVE_NO_DAEMON": "1"}
proc = subprocess.Popen(
    [binary, "--db", db_path, "--no-embed", "--log", "error"],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env
)

# Initialize
msg = json.dumps({"jsonrpc":"2.0","id":1,"method":"initialize",
                  "params":{"protocolVersion":"2024-11-05","capabilities":{},
                            "clientInfo":{"name":"rss-probe","version":"0.1"}}}) + "\n"
proc.stdin.write(msg.encode())
proc.stdin.flush()
proc.stdout.readline()  # consume response

# Server is now idle and ready
time.sleep(0.3)  # settle

import subprocess as sp
rss_out = sp.run(["ps", "-o", "rss=", "-p", str(proc.pid)], capture_output=True, text=True)
rss_kb = rss_out.stdout.strip()
print(f"IDLE_RSS_EMPTY_DB_KB={rss_kb}")

proc.stdin.close()
proc.wait(timeout=5)
PYEOF
echo ""

echo "--- Idle RSS: seeded DB ---"
SEEDED_DB="$PERF_DIR/seeded.db"
rm -f "$SEEDED_DB"

# Seed: create 10 entities and 5 notes via smoke test protocol
python3 - "$BINARY" "$SEEDED_DB" <<'PYEOF'
import sys, json, subprocess, os

binary = sys.argv[1]
db_path = sys.argv[2]

env = {**os.environ, "KHIVE_NO_DAEMON": "1"}
proc = subprocess.Popen(
    [binary, "--db", db_path, "--no-embed", "--log", "error"],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env
)

req_id = 0
def send(method, params=None):
    global req_id
    req_id += 1
    msg = {"jsonrpc":"2.0","id":req_id,"method":method}
    if params: msg["params"] = params
    proc.stdin.write((json.dumps(msg)+"\n").encode())
    proc.stdin.flush()

def recv():
    return json.loads(proc.stdout.readline())

def call(name, args):
    ops = json.dumps([{"tool": name, "args": args}])
    send("tools/call", {"name":"request","arguments":{"ops":ops}})
    resp = recv()
    results = resp["result"]["content"]
    body = json.loads(results[0]["text"])
    return body["results"][0]["result"]

# Initialize
send("initialize", {"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"seed","version":"0.1"}})
recv()
proc.stdin.write((json.dumps({"jsonrpc":"2.0","method":"notifications/initialized"})+"\n").encode())
proc.stdin.flush()

# Seed 10 entities
for i in range(10):
    call("create", {"kind":"entity","entity_kind":"concept","name":f"SeedConcept{i}","description":f"Seed entity {i} for perf baseline"})

# Seed 5 notes
for i in range(5):
    call("create", {"kind":"note","note_kind":"observation","content":f"Seed observation {i}: performance measurement baseline data for khive engine","salience":0.7})

# Seed 3 edges
entities = call("list", {"kind":"entity","entity_kind":"concept"})
if len(entities) >= 2:
    for i in range(min(3, len(entities)-1)):
        try:
            call("link", {"source_id":entities[i]["id"],"target_id":entities[i+1]["id"],"relation":"related_to","weight":0.5})
        except: pass

print(f"Seeded DB at {db_path}: 10 entities, 5 notes, up to 3 edges")
proc.stdin.close()
proc.wait(timeout=5)
PYEOF

echo ""

# Now measure RSS with seeded DB
python3 - "$BINARY" "$SEEDED_DB" <<'PYEOF'
import sys, json, subprocess, time, os

binary = sys.argv[1]
db_path = sys.argv[2]

env = {**os.environ, "KHIVE_NO_DAEMON": "1"}
proc = subprocess.Popen(
    [binary, "--db", db_path, "--no-embed", "--log", "error"],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env
)

msg = json.dumps({"jsonrpc":"2.0","id":1,"method":"initialize",
                  "params":{"protocolVersion":"2024-11-05","capabilities":{},
                            "clientInfo":{"name":"rss-probe","version":"0.1"}}}) + "\n"
proc.stdin.write(msg.encode())
proc.stdin.flush()
proc.stdout.readline()

time.sleep(0.3)

import subprocess as sp
rss_out = sp.run(["ps", "-o", "rss=", "-p", str(proc.pid)], capture_output=True, text=True)
rss_kb = rss_out.stdout.strip()
print(f"IDLE_RSS_SEEDED_DB_KB={rss_kb}")

proc.stdin.close()
proc.wait(timeout=5)
PYEOF
echo ""

echo "=== Measurement complete ==="
echo "Log: $LOG"
