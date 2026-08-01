# The full pack set the installed daemon must serve. `make local` verifies the
# INSTALLED binary registers the whole set by positive artifact (a verbs()
# count from the binary itself) — a build that silently drops a pack's
# inventory registration fails here instead of shipping.
FULL_PACKS := kg,gtd,memory,comm,schedule,session,workspace,blob,git,knowledge,brain,code,formal

.PHONY: check clippy test contract-test fmt fmt-check build clean ci docs-check publish publish-dry local check-fwd bench-1m bench-1m-ci hold-time-gate

check:
	cd crates && cargo check --workspace

clippy:
	cd crates && cargo clippy --workspace --all-targets --all-features -- -D warnings

test:
	cd crates && cargo test --workspace

contract-test:
	cd crates && cargo build --release -p kkernel
	python3 tests/contract_test.py

fmt:
	cd crates && cargo fmt --all
	deno fmt docs/

fmt-check:
	cd crates && cargo fmt --all -- --check

build:
	cd crates && cargo build --workspace --release

clean:
	cd crates && cargo clean

docs-check:
	deno fmt --check docs/

check-fwd:
	RUSTFLAGS="-D warnings" cargo check --manifest-path crates/khive-merge/Cargo.toml --all-targets
	cargo clippy --manifest-path crates/khive-merge/Cargo.toml --all-targets -- -D warnings
	cargo test --manifest-path crates/khive-merge/Cargo.toml

ci:
	./scripts/ci.sh

publish-dry:
	./scripts/publish.sh

publish:
	./scripts/publish.sh --live

bench-1m:
	@echo "==> Running Vamana 1M scale-proof bench (3-point: 100K/316K/1M, ~7 min)..."
	@echo "    Set SIFT_DIR to the sift_base.fvecs / sift_query.fvecs directory."
	bash scripts/bench_1m.sh

bench-1m-ci:
	@echo "==> Running Vamana CI smoke bench (2-point: 10K/50K, <60 s)..."
	@echo "    Set SIFT_DIR to the sift_base.fvecs / sift_query.fvecs directory."
	bash scripts/bench_1m.sh --ci

hold-time-gate:
	@echo "==> ADR-135 F4 release gate: per-shape writer hold-time regression coverage..."
	cd crates && cargo test -p khive-pack-comm --test hold_time_regression -- --nocapture

local:
	@echo "==> Building kkernel (release, channel-email, channel-telegram)..."
	@cargo build --release -p kkernel --features channel-email,channel-telegram --manifest-path crates/Cargo.toml
	@SRC=$${CARGO_TARGET_DIR:-crates/target}/release/kkernel; \
	DEST=$$HOME/.cargo/bin/kkernel; \
	if [ ! -f "$$SRC" ]; then echo "==> ERROR: build artifact $$SRC missing"; exit 1; fi; \
	SRC_HASH=$$(md5 -q "$$SRC"); \
	SRC_SIZE=$$(stat -f '%z' "$$SRC"); \
	echo "==> Source:  $$SRC ($$SRC_HASH, $$SRC_SIZE bytes)"; \
	echo "==> Staging + codesigning $$DEST.new..."; \
	cp "$$SRC" "$$DEST.new"; \
	codesign -s - -f "$$DEST.new" 2>/dev/null || true; \
	STAGED_HASH=$$(md5 -q "$$DEST.new"); \
	echo "==> Atomically moving into place..."; \
	mv "$$DEST.new" "$$DEST"; \
	echo "==> Killing running kkernel daemon (bridges respawn the NEW binary and self-heal via re-exec)..."; \
	pkill -f 'kkernel mcp --daemon' 2>/dev/null || true; \
	for i in 1 2 3 4 5; do \
	  if pgrep -f 'kkernel mcp --daemon' >/dev/null 2>&1; then sleep 1; else break; fi; \
	done; \
	if pgrep -f 'kkernel mcp --daemon' >/dev/null 2>&1; then \
	  echo "==> WARNING: daemon still running after 5s — SIGKILL"; \
	  pkill -9 -f 'kkernel mcp --daemon' 2>/dev/null || true; \
	  sleep 1; \
	fi; \
	DEST_HASH=$$(md5 -q "$$DEST"); \
	DEST_SIZE=$$(stat -f '%z' "$$DEST"); \
	DEST_MTIME=$$(stat -f '%Sm' "$$DEST"); \
	if [ "$$STAGED_HASH" != "$$DEST_HASH" ]; then \
	  echo "==> ERROR: post-mv hash drift! staged=$$STAGED_HASH dest=$$DEST_HASH"; \
	  exit 1; \
	fi; \
	echo "==> Verifying installed binary loads the full pack set..."; \
	PROBE_OUT=$$(mktemp); \
	if KHIVE_NO_DAEMON=1 KHIVE_PACKS="$(FULL_PACKS)" \
	  perl -e 'alarm 120; exec @ARGV or die' -- "$$DEST" exec --output-format json 'verbs()' > "$$PROBE_OUT" 2>/dev/null; then \
	  VERBS=$$(python3 -c 'import json,sys; r=json.load(sys.stdin); e=r["results"][0]; v=e["result"]["verbs"]; sys.exit(1) if (e.get("ok") is not True or not isinstance(v, list)) else print(len(v))' < "$$PROBE_OUT" 2>/dev/null || echo 0); \
	else \
	  VERBS=0; \
	fi; \
	rm -f "$$PROBE_OUT"; \
	if [ "$$VERBS" -lt 80 ]; then \
	  echo "==> ERROR: installed binary registered $$VERBS verbs (expected >= 80) — full pack set did not load"; \
	  exit 1; \
	fi; \
	echo "==> Installed: $$DEST ($$DEST_HASH, $$DEST_SIZE bytes, $$DEST_MTIME, $$VERBS verbs)"; \
	"$$DEST" --version
	@echo "==> Done. Run /mcp in Claude Code to reconnect."
