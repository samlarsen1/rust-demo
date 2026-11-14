# Fallback for machines without `just`. Mirrors the justfile.
.PHONY: default fmt lint test audit demo hooks check

default: check

fmt:
	cargo fmt --all

lint:
	cargo fmt --all -- --check
	cargo clippy --all-targets --all-features -- -D warnings

test:
	cargo test --all-targets

audit:
	@if command -v cargo-audit >/dev/null; then cargo audit --deny warnings; else echo "cargo-audit not installed: cargo install --locked cargo-audit"; fi
	@if command -v cargo-deny >/dev/null; then cargo deny check --all-features; else echo "cargo-deny not installed: cargo install --locked cargo-deny"; fi

demo:
	cargo run --quiet -- demo

hooks:
	pre-commit install

check: lint test audit
