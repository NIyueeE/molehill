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

# Install benchmark system prerequisites (iperf3, tc/netem; python deps come
# from the PEP 723 headers via uv — install uv itself with: curl -LsSf
# https://astral.sh/uv/install.sh | sh)
bench-deps:
    sudo apt-get install -y iperf3 iproute2

# Fetch the latest GitHub release binaries of the peer tools (frp, rathole,
# bore) into /tmp/bench-peers — nothing is built from source.
bench-peers:
    uv run benches/scripts/bench/fetch_peers.py

# Run the full benchmark matrix (molehill arms at full rigor; loss cells need netem).
bench:
    uv run benches/scripts/bench/bench.py

# Render the README chart + markdown tables from the latest results file.
bench-plot:
    uv run benches/scripts/bench/plot_bench.py

# Regression gate: latest results vs the previous tag's file (pre-tag ritual).
bench-check:
    uv run benches/scripts/bench/check_regression.py

# Lint the python bench/test entries (ruff via uvx; also in the pre-commit gate).
py-lint:
    uvx ruff check benches/scripts/

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
