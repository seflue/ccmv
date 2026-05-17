set shell := ["bash", "-uc"]

default:
    @just --list

check:
    cargo check --all-targets

build:
    cargo build --release

test:
    cargo test --all-features

lint:
    cargo clippy --all-targets --all-features -- -D warnings

# Format + auto-apply clippy suggestions
fix:
    cargo fmt --all
    cargo clippy --all-targets --all-features --fix --allow-dirty --allow-staged

# Pre-push gate: same checks CI runs
ci:
    cargo fmt --all -- --check
    cargo clippy --all-targets --all-features -- -D warnings
    cargo test --all-features

install:
    cargo install --path . --locked

# Release: pre-flight, bump, wait for CI green, tag.
# See scripts/release.sh for details.
release version:
    scripts/release.sh {{version}}
