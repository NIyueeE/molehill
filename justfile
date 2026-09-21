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

# Run only the release review (githooks/pre-tag) without creating a tag.
tag-check:
    githooks/pre-tag

# Release review + create the local annotated v* tag for Cargo.toml's version.
# Pushing the tag is the deliberate release act — the review fires again
# inside pre-push; see AGENTS.md §5 and docs/release.md.
tag:
    #!/usr/bin/env bash
    set -euo pipefail
    cd "$(git rev-parse --show-toplevel)"
    version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -1)
    githooks/pre-tag --tag "v${version}"
    git tag -a "v${version}" -m "molehill v${version}"
    echo "tagged v${version} — push it deliberately: git push origin v${version}"

# Check all feature combinations (CI: features job; requires cargo-hack).
powerset:
    cargo hack check --feature-powerset --no-dev-deps

# Install benchmark system prerequisites (iperf3, tc/netem; python deps come
# from the PEP 723 headers via uv — install uv itself with: curl -LsSf
# https://astral.sh/uv/install.sh | sh)
bench-deps:
    sudo apt-get install -y iperf3 iproute2

# Fetch the latest GitHub release binaries of the peer tools (frp, rathole,
# bore) into ~/tmp/bench-peers — nothing is built from source.
bench-peers:
    uv run benches/scripts/bench/fetch_peers.py

# Run the full benchmark matrix (molehill arms at full rigor; loss cells need netem).
bench:
    uv run benches/scripts/bench/bench.py

# Quick perf sanity for the dev loop: molehill only, loopback + loss1 cells,
# one rep, short durations. Writes results-dev.json — excluded from the
# version-picked files plot/regression use, so it can never pollute a
# release baseline.
bench-fast:
    MOLEHILL_REPS=1 MOLEHILL_SECS=4 MOLEHILL_SECS_WEAK=6 uv run benches/scripts/bench/bench.py --tools=molehill --cells=0/0,1%/10 --variants=mux,noise,kcp4 --fresh --out benches/scripts/bench/results-dev.json

# Render the README chart + markdown tables from the latest results file.
bench-plot:
    uv run benches/scripts/bench/plot_bench.py

# Regression gate: latest results vs the previous tag's file (pre-tag ritual).
bench-check:
    uv run benches/scripts/bench/check_regression.py

# Fast dev loop: lib tests + the core integration subset (~1 min; the full
# suite is ~72 s and runs on every push/CI — see docs/checks.md).
test-fast:
    cargo test --lib --quiet
    cargo test --test integration_test -- --test-threads=1 tcp udp per_service_data_modes mixed_transports per_service_transport

# Lint the python bench/test entries (ruff via uvx; also in the pre-commit gate).
py-lint:
    uvx ruff check benches/scripts/

# Build the scratch container image from a release musl binary (see Containerfile).
container:
    #!/usr/bin/env bash
    set -euo pipefail
    cd "$(git rev-parse --show-toplevel)"
    cargo build --release --target x86_64-unknown-linux-musl --no-default-features --features server,client,noise,hot-reload,multiplex,kcp
    mkdir -p img/bin/amd64
    cp target/x86_64-unknown-linux-musl/release/molehill img/bin/amd64/
    docker build -f Containerfile -t molehill img/
