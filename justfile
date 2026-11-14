# Task runner — `just --list`. A Makefile mirrors these targets.

default: check

# Format the code base
fmt:
    cargo fmt --all

# Strict lints (warnings are errors)
lint:
    cargo fmt --all -- --check
    cargo clippy --all-targets --all-features -- -D warnings

# Unit, property and integration tests
test:
    cargo test --all-targets

# Supply chain: RustSec advisories, licenses, duplicate crates
audit:
    @if command -v cargo-audit >/dev/null; then cargo audit --deny warnings; else echo "cargo-audit not installed: cargo install --locked cargo-audit"; fi
    @if command -v cargo-deny >/dev/null; then cargo deny check --all-features; else echo "cargo-deny not installed: cargo install --locked cargo-deny"; fi

# Cold + incremental sync demo with full report
demo:
    cargo run --quiet -- demo

# Install git hooks
hooks:
    pre-commit install

# Everything that must pass before a task is done
check: lint test audit
