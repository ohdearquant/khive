# The full pack set the installed daemon must serve. `make local` verifies the
# freshly built artifact before installation by running verbs() against that
# exact binary. The floor is the current 90-verb production surface: additions
# pass without a Makefile change, while any loss remains fail-closed.
FULL_PACKS := kg,gtd,memory,comm,schedule,session,workspace,blob,git,knowledge,brain,code,formal
LOCAL_VERB_FLOOR := 90
CARGO ?= cargo
LOCAL_BUILD_RECEIPT := crates/target/khive-local-build.json
LOCAL_VERIFY_STAMP := $(LOCAL_BUILD_RECEIPT).verified

.PHONY: check clippy test contract-test fmt fmt-check build build-local verify-local-artifact clean ci docs-check publish publish-dry local check-fwd bench-1m bench-1m-ci hold-time-gate eval-retrieval-gold-check

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

build-local:
	@echo "==> Building kkernel (release, channel-email, channel-telegram)..."
	@python3 scripts/build_local_artifact.py \
		--cargo "$(CARGO)" \
		--manifest-path crates/Cargo.toml \
		--package kkernel \
		--features channel-email,channel-telegram \
		--receipt "$(LOCAL_BUILD_RECEIPT)"

verify-local-artifact: build-local
	@python3 scripts/verify_local_artifact.py \
		--build-receipt "$(LOCAL_BUILD_RECEIPT)" \
		--packs "$(FULL_PACKS)" \
		--min-verbs "$(LOCAL_VERB_FLOOR)" \
		--stamp "$(LOCAL_VERIFY_STAMP)"

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

eval-retrieval-gold-check:
	@echo "==> Retrieval eval harness: re-running A_fused_direct against committed gold..."
	@echo "    (tolerance 0.002 absorbs a pre-existing rank-10-boundary tie-break jitter"
	@echo "    in memory.recall unrelated to this harness's temporal-weight hermeticity fix"
	@echo "    -- see eval/retrieval/README.md Determinism section)"
	cd eval/retrieval && uv run python evaluate.py --check-gold --gold-tolerance 0.002

hold-time-gate:
	@echo "==> ADR-135 F4 release gate: per-shape writer hold-time regression coverage..."
	cd crates && cargo test -p khive-pack-comm --test hold_time_regression -- --nocapture

local: verify-local-artifact
	@if ! VERIFIED_ASSIGNMENTS=$$(python3 scripts/verify_local_artifact.py \
	  --build-receipt "$(LOCAL_BUILD_RECEIPT)" \
	  --inspect-stamp "$(LOCAL_VERIFY_STAMP)" \
	  --min-verbs "$(LOCAL_VERB_FLOOR)"); then \
	  exit 1; \
	fi; \
	if ! eval "$$VERIFIED_ASSIGNMENTS"; then \
	  echo "==> ERROR: could not load verified-artifact fields"; \
	  exit 1; \
	fi; \
	DEST=$$HOME/.cargo/bin/kkernel; \
	if [ ! -f "$$SRC" ]; then echo "==> ERROR: build artifact $$SRC missing"; exit 1; fi; \
	SRC_SHA256=$$({ shasum -a 256 "$$SRC" 2>/dev/null || sha256sum "$$SRC"; } | awk '{print $$1}'); \
	if [ "$$VERIFIED_SHA256" != "$$SRC_SHA256" ]; then \
	  echo "==> ERROR: build artifact changed after verification! verified=$$VERIFIED_SHA256 current=$$SRC_SHA256"; \
	  exit 1; \
	fi; \
	SRC_HASH=$$(md5 -q "$$SRC"); \
	SRC_SIZE=$$(stat -f '%z' "$$SRC"); \
	echo "==> Source:  $$SRC ($$SRC_HASH, $$SRC_SIZE bytes, $$VERIFIED_VERBS verified verbs)"; \
	echo "==> Staging + codesigning $$DEST.new..."; \
	cp "$$SRC" "$$DEST.new"; \
	COPIED_SHA256=$$({ shasum -a 256 "$$DEST.new" 2>/dev/null || sha256sum "$$DEST.new"; } | awk '{print $$1}'); \
	if [ "$$VERIFIED_SHA256" != "$$COPIED_SHA256" ]; then \
	  echo "==> ERROR: staged bytes differ from the verified build artifact! verified=$$VERIFIED_SHA256 staged=$$COPIED_SHA256"; \
	  rm -f "$$DEST.new"; \
	  exit 1; \
	fi; \
	if ! codesign -s - -f "$$DEST.new"; then \
	  echo "==> ERROR: codesign failed on $$DEST.new — refusing to install"; \
	  rm -f "$$DEST.new"; \
	  exit 1; \
	fi; \
	echo "==> Re-verifying the SIGNED artifact (codesign rewrites the file, so the pre-sign verification does not cover the bytes that get installed)..."; \
	if ! python3 scripts/verify_local_artifact.py \
	  --artifact "$$DEST.new" \
	  --packs "$(FULL_PACKS)" \
	  --min-verbs "$(LOCAL_VERB_FLOOR)" >/dev/null; then \
	  echo "==> ERROR: signed artifact failed verification — refusing to install"; \
	  rm -f "$$DEST.new"; \
	  exit 1; \
	fi; \
	SIGNED_SHA256=$$({ shasum -a 256 "$$DEST.new" 2>/dev/null || sha256sum "$$DEST.new"; } | awk '{print $$1}'); \
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
	DEST_SHA256=$$({ shasum -a 256 "$$DEST" 2>/dev/null || sha256sum "$$DEST"; } | awk '{print $$1}'); \
	if [ "$$SIGNED_SHA256" != "$$DEST_SHA256" ]; then \
	  echo "==> ERROR: installed bytes differ from the verified signed artifact! signed=$$SIGNED_SHA256 installed=$$DEST_SHA256"; \
	  exit 1; \
	fi; \
	echo "==> Installed: $$DEST ($$DEST_HASH, $$DEST_SIZE bytes, $$DEST_MTIME, $$VERIFIED_VERBS verified verbs)"; \
	"$$DEST" --version
	@echo "==> Done. Run /mcp in Claude Code to reconnect."
