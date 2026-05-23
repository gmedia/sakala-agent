.PHONY: fmt fmt-check lint test check run build audit

fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all -- --check

lint:
	cargo clippy --workspace --all-targets --all-features -- -D warnings

test:
	cargo test --workspace --all-features

check: fmt-check lint test build

run:
	cargo run -p sakala-agent

build:
	cargo build --workspace --all-features

audit:
	cargo audit
