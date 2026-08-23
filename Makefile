# The full pack set the installed daemon must serve. `make local` verifies the
# freshly built artifact before installation by running verbs() against that
# exact binary. The floor is the current 90-verb production surface: additions
# pass without a Makefile change, while any loss remains fail-closed.
FULL_PACKS := kg,gtd,memory,comm,schedule,session,workspace,blob,git,knowledge,brain,code,formal
LOCAL_VERB_FLOOR := 90
CARGO ?= cargo
LOCAL_BUILD_RECEIPT := crates/target/khive-local-build.json
FLEET_ARTIFACT ?=
# Every variable below can be overridden on the command line (`make VAR=...`)
# and plain `:=`/`?=` loses to command-line assignments. Make performs no
# shell escaping on `$(VAR)`, so a caller-controlled value spliced directly
# into a recipe's shell text (e.g. `$(LOCAL_VERB_FLOOR)`) could break out of
# a quoted argument and run as shell code. Capture the caller's literal value
# before make can expand any `$` bytes, then pass it to the recipe through
# the environment instead of shell source — recipes read `$$<VAR>_VALUE`,
# never `$(VAR)`. `FULL_PACKS` and `LOCAL_VERB_FLOOR` are additionally gated
# by `validate-make-inputs` so a hostile value is rejected before it reaches
# a shell at all; the path/tool variables (LOCAL_BUILD_RECEIPT, CARGO) need no
# character allowlist — the env-value pass alone removes the shell-parse
# surface — but must never be re-spliced as `$(VAR)` into recipe shell text.
# The verification stamp path is likewise never a `:=` derivation — it is
# `"$${LOCAL_BUILD_RECEIPT_VALUE}.verified"`, computed by the shell at recipe
# time from the already-captured env value, so a literal `$` in the receipt
# path is inert data and no `$(shell ...)` payload can run during parsing.
override FLEET_ARTIFACT_VALUE := $(value FLEET_ARTIFACT)
unexport FLEET_ARTIFACT
export FLEET_ARTIFACT_VALUE
override FULL_PACKS_VALUE := $(value FULL_PACKS)
unexport FULL_PACKS
export FULL_PACKS_VALUE
override LOCAL_VERB_FLOOR_VALUE := $(value LOCAL_VERB_FLOOR)
unexport LOCAL_VERB_FLOOR
export LOCAL_VERB_FLOOR_VALUE
override LOCAL_BUILD_RECEIPT_VALUE := $(value LOCAL_BUILD_RECEIPT)
unexport LOCAL_BUILD_RECEIPT
export LOCAL_BUILD_RECEIPT_VALUE
override CARGO_VALUE := $(value CARGO)
unexport CARGO
export CARGO_VALUE

.PHONY: check clippy test contract-test fmt fmt-check build build-local verify-local-artifact validate-make-inputs fleet-build fleet-check clean ci docs-check publish publish-dry local check-fwd bench-1m bench-1m-ci hold-time-gate eval-retrieval-gold-check

check:
	cd crates && cargo check --workspace

clippy:
	cd crates && cargo clippy --workspace --all-targets --all-features -- -D warnings

test:
	cd crates && cargo test --workspace

# The all-zone chrono-tz sweep. Ignored by default because it walks every
# zone in the bundled database and costs ~50s; it exists to be RUN when the
# chrono-tz pin moves, which is when the bundled zone data can change under
# the resolver. Wired to CI on the manifests that carry that pin, so the
# duty is triggered rather than remembered.
tz-audit:
	cd crates && $(CARGO) test -p khive-pack-gtd --lib tz_database_audit -- --ignored

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
		--cargo "$$CARGO_VALUE" \
		--manifest-path crates/Cargo.toml \
		--package kkernel \
		--features channel-email,channel-telegram \
		--receipt "$$LOCAL_BUILD_RECEIPT_VALUE"

# Reject a FULL_PACKS/LOCAL_VERB_FLOOR value (from the Makefile default or a
# caller override) that contains anything outside its allowlisted character
# class. Every recipe below reads the sanitized value from the `*_VALUE`
# shell environment variables, never via a raw `$(FULL_PACKS)` /
# `$(LOCAL_VERB_FLOOR)` splice, so this gate is what stands between a
# hostile override and a shell.
validate-make-inputs:
	@case "$$FULL_PACKS_VALUE" in \
		"") echo "==> ERROR: FULL_PACKS is empty" >&2; exit 1 ;; \
		*[!A-Za-z0-9_,-]*) echo "==> ERROR: FULL_PACKS contains characters outside the allowed [A-Za-z0-9_,-] set: $$FULL_PACKS_VALUE" >&2; exit 1 ;; \
	esac
	@case "$$LOCAL_VERB_FLOOR_VALUE" in \
		"") echo "==> ERROR: LOCAL_VERB_FLOOR is empty" >&2; exit 1 ;; \
		*[!0-9]*) echo "==> ERROR: LOCAL_VERB_FLOOR must contain digits only: $$LOCAL_VERB_FLOOR_VALUE" >&2; exit 1 ;; \
	esac

verify-local-artifact: validate-make-inputs build-local
	@python3 scripts/verify_local_artifact.py \
		--build-receipt "$$LOCAL_BUILD_RECEIPT_VALUE" \
		--packs "$$FULL_PACKS_VALUE" \
		--min-verbs "$$LOCAL_VERB_FLOOR_VALUE" \
		--stamp "$${LOCAL_BUILD_RECEIPT_VALUE}.verified"

# Build and verify the release artifact without installing it or interrupting
# the serving daemon. This compatibility name makes the build-only safety gate
# discoverable independently of the local-install recipe.
fleet-build: verify-local-artifact

# Re-run the verification probe without rebuilding. By default this checks the
# exact Cargo artifact named by the current build receipt. Set FLEET_ARTIFACT to
# check any executable directly, including the installed kkernel binary:
#   make fleet-check FLEET_ARTIFACT="$HOME/.cargo/bin/kkernel"
fleet-check: validate-make-inputs
	@if [ -n "$$FLEET_ARTIFACT_VALUE" ]; then \
		python3 scripts/verify_local_artifact.py \
			--artifact "$$FLEET_ARTIFACT_VALUE" \
			--packs "$$FULL_PACKS_VALUE" \
			--min-verbs "$$LOCAL_VERB_FLOOR_VALUE"; \
	else \
		python3 scripts/verify_local_artifact.py \
			--build-receipt "$$LOCAL_BUILD_RECEIPT_VALUE" \
			--packs "$$FULL_PACKS_VALUE" \
			--min-verbs "$$LOCAL_VERB_FLOOR_VALUE"; \
	fi

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
	@echo "    -- see benches/retrieval/README.md Determinism section)"
	cd benches/retrieval && uv run python evaluate.py --check-gold --gold-tolerance 0.002

hold-time-gate:
	@echo "==> ADR-135 F4 release gate: per-shape writer hold-time regression coverage..."
	cd crates && cargo test -p khive-pack-comm --test hold_time_regression -- --nocapture

local: verify-local-artifact
	@if ! VERIFIED_ASSIGNMENTS=$$(python3 scripts/verify_local_artifact.py \
	  --build-receipt "$$LOCAL_BUILD_RECEIPT_VALUE" \
	  --inspect-stamp "$${LOCAL_BUILD_RECEIPT_VALUE}.verified" \
	  --min-verbs "$$LOCAL_VERB_FLOOR_VALUE"); then \
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
	  --packs "$$FULL_PACKS_VALUE" \
	  --min-verbs "$$LOCAL_VERB_FLOOR_VALUE" >/dev/null; then \
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
