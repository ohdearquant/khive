.PHONY: check clippy test contract-test fmt fmt-check build clean ci docs-check publish publish-dry local proof-check

check:
	cd crates && cargo check --workspace

clippy:
	cd crates && cargo clippy --workspace -- -D warnings

test:
	cd crates && cargo test --workspace

contract-test:
	cd crates && cargo build --release -p khive-mcp
	python3 tests/contract_test.py

fmt:
	cd crates && cargo fmt --all
	deno fmt docs/

fmt-check:
	cd crates && cargo fmt --all -- --check

build:
	cd crates && cargo build --workspace --release

proof-check:
	./scripts/check-proof-references.sh

clean:
	cd crates && cargo clean

docs-check:
	deno fmt --check docs/

ci:
	./scripts/ci.sh

publish-dry:
	./scripts/publish.sh

publish:
	./scripts/publish.sh --live

local:
	@echo "==> Building khive-mcp (release)..."
	cd crates && cargo build --release -p khive-mcp
	@echo "==> Killing running khive-mcp processes..."
	-pkill -f 'khive-mcp' 2>/dev/null || true
	@sleep 1
	@echo "==> Installing to ~/.cargo/bin/khive-mcp..."
	cp crates/target/release/khive-mcp ~/.cargo/bin/khive-mcp
	@echo "==> Codesigning..."
	codesign -s - -f ~/.cargo/bin/khive-mcp 2>/dev/null || true
	@echo "==> Done. Run /mcp in Claude Code to reconnect."
