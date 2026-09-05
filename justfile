# Task runner for the molehill repo.
# `just` (no arguments) lists all recipes.

default:
    @just --list

# One-time setup per clone: activate git hooks + install missing check tools.
setup:
    #!/usr/bin/env bash
    set -euo pipefail
    cd "$(git rev-parse --show-toplevel)"
    git config core.hooksPath githooks
    echo "hooksPath -> githooks"
    for tool in cargo-machete cargo-audit cargo-outdated cargo-deny; do
        if command -v "$tool" >/dev/null 2>&1; then
            echo "ok:      $tool"
        else
            echo "install: $tool"
            cargo install "$tool" --locked
        fi
    done
    echo "setup complete"

# Auto-fix formatting across the workspace.
fmt:
    cargo fmt --all

# Run tests (serial by design — the integration suite binds fixed ports).
test:
    cargo test -- --test-threads=1

# Run the full check chain (identical to hooks + CI: fmt/secrets/machete/docs/clippy + audit/deny/outdated/test).
check:
    #!/usr/bin/env bash
    set -euo pipefail
    cd "$(git rev-parse --show-toplevel)"
    githooks/pre-commit
    githooks/pre-push

# Check all feature combinations (CI: features job; requires cargo-hack).
powerset:
    cargo hack check --feature-powerset --no-dev-deps --mutually-exclusive-features default,native-tls,websocket-native-tls,rustls,websocket-rustls

# Build the scratch container image from a release musl binary (see Containerfile).
container:
    #!/usr/bin/env bash
    set -euo pipefail
    cd "$(git rev-parse --show-toplevel)"
    cargo build --release --target x86_64-unknown-linux-musl --no-default-features --features server,client,rustls,noise,websocket-rustls,hot-reload,multiplex
    mkdir -p img/bin/amd64
    cp target/x86_64-unknown-linux-musl/release/molehill img/bin/amd64/
    cp /etc/ssl/certs/ca-certificates.crt img/bin/amd64/
    docker build -f Containerfile -t molehill img/
