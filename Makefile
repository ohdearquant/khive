.PHONY: check clippy test fmt fmt-check build clean ci docs-check

check:
	cd crates && cargo check --workspace

clippy:
	cd crates && cargo clippy --workspace -- -D warnings

test:
	cd crates && cargo test --workspace

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
