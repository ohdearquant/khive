.PHONY: check clippy test contract-test fmt fmt-check build clean ci docs-check publish publish-dry

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
