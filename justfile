# Task runner for the molehill repo.
# `just` (no arguments) lists all recipes.

default:
    @just --list

# One-time setup per clone: activate git hooks + install missing check tools
# (the python gates run from the bench project through uv; `just powerset`
# needs cargo-hack).
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
    if command -v uv >/dev/null 2>&1; then
        echo "ok:      uv"
    else
        echo "install: uv   (the python bench gates; run:"
        echo "          curl -LsSf https://astral.sh/uv/install.sh | sh)"
    fi
    if command -v cargo-hack >/dev/null 2>&1; then
        echo "ok:      cargo-hack"
    else
        echo "missing: cargo-hack (needed by 'just powerset'; install: cargo install cargo-hack --locked)"
    fi
    echo "setup complete"

# Auto-fix formatting across the workspace.
fmt:
    cargo fmt --all

# Run tests (serial by design — the integration suite binds fixed ports).
test:
    cargo test -- --test-threads=1

# Run the full check chain (identical to hooks + CI: fmt/secrets/machete/docs/ruff/clippy + audit/deny/outdated/test).
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

# Install benchmark system prerequisites (iperf3, tc/netem; the python
# environment comes from `bench/uv.lock` through uv — install uv itself with:
# curl -LsSf https://astral.sh/uv/install.sh | sh)
bench-deps:
    sudo apt-get install -y iperf3 iproute2

# Interop matrix: this build against the previous release's binary, both
# directions (needs network on the first run; the fetched binary is cached
# under ~/tmp/interop). Override the peer with MOLEHILL_OLD_TAG=vX.Y.Z.
# See docs/checks.md, "Outside the chain".
interop:
    #!/usr/bin/env bash
    set -euo pipefail
    cd "$(git rev-parse --show-toplevel)"
    export UV_PROJECT_ENVIRONMENT="${UV_PROJECT_ENVIRONMENT:-$HOME/.cache/molehill-bench/venv}"
    export MOLEHILL_OLD_BIN="$(
        uv run --project bench --locked bench interop | sed -n 's/^MOLEHILL_OLD_BIN=//p'
    )"
    test -x "$MOLEHILL_OLD_BIN" || { echo "fetch did not yield a binary" >&2; exit 1; }
    # The cases are #[ignore]d so a plain `cargo test` reports them as not run
    # (libtest captures a passing test's output); this is the run that does them.
    cargo test --test interop_test -- --test-threads=1 --include-ignored

# Transparent-L3 acceptance: the real binaries in a three-namespace topology,
# proving the client (never the server) owns the public ip:port. Needs root
# for the veth pairs, TUN devices and routes, so it is outside the check
# chain; without root it skips loudly (exit 77) instead of passing silently.
# See docs/checks.md, "Outside the chain".
l3-accept *ARGS:
    just bench l3 {{ARGS}}

# Transparent L3 IPv6 acceptance: root-only Linux, a focused companion to
# `l3-accept` (see docs/checks.md).
l3-accept-ipv6 *ARGS:
    bash bench/src/bench/utils/l3/run_ipv6.sh {{ARGS}}

# The performance model: one topology, a declared metric registry, a control
# arm in every scenario and a noise floor the run measures itself. Root-only
# (three network namespaces) and outside the check chain; results land outside
# the tree. Method: docs/benchmarks.md, "The bench model".
#
# This is the only human entry point. Every subcommand passes through, and
# `plot` gets the optional matplotlib environment automatically:
#   sudo -n just bench                      # the smoke profile, about a minute
#   sudo -n just bench --profile dev --aa   # the default for an optimization
#   sudo -n just bench --arm id=l3,txqueuelen=10000
#   sudo -n just bench --ab-arm l3 --binary-b /path/to/other/build
#   just bench list | report FILE | gate FILE | compare A B | doctor
#   just bench peers fetch                  # the reference-tool binaries
#   just bench interop | udp-stress | l3 | l3-census | mux
#   just bench plot ~/tmp/bench-*.json --out-dir ~/tmp/charts
bench *ARGS:
    #!/usr/bin/env bash
    set -euo pipefail
    cd "$(git rev-parse --show-toplevel)"
    # A run under sudo must not create a root-owned .venv in the checkout.
    export UV_PROJECT_ENVIRONMENT="${UV_PROJECT_ENVIRONMENT:-$HOME/.cache/molehill-bench/venv}"
    set -- {{ARGS}}
    extra=()
    if [ "${1:-}" = "plot" ]; then
        extra=(--extra plot)
    fi
    exec uv run --project bench --locked "${extra[@]}" bench "$@"

# The model's own checks (metric registry, verdict rules, comparability rules,
# probe syntax): no root, no topology, seconds. Also in the pre-commit gate.
bench-selfcheck:
    #!/usr/bin/env bash
    set -euo pipefail
    cd "$(git rev-parse --show-toplevel)"
    export UV_PROJECT_ENVIRONMENT="${UV_PROJECT_ENVIRONMENT:-$HOME/.cache/molehill-bench/venv}"
    exec uv run --project bench --locked bench selfcheck

# Fast dev loop: lib tests + the core integration subset (~1 min; the full
# suite is ~72 s and runs on every push/CI — see docs/checks.md).
test-fast:
    cargo test --lib --quiet
    cargo test --test integration_test -- --test-threads=1 tcp udp per_service_data_modes mixed_transports per_service_transport

# Lint and format-check the bench project (ruff from its lint group; also in
# the pre-commit gate).
py-lint:
    uv run --project bench --locked --group lint ruff check bench
    uv run --project bench --locked --group lint ruff format --check bench

# Typecheck the bench project (pyright standard; the adapter layer is strict).
py-typecheck:
    cd bench && uv run --locked --extra plot --group typecheck pyright

# Auto-fix the bench project's formatting (ruff format).
py-fmt:
    uv run --project bench --locked --group lint ruff format bench

# Build the scratch container image from a release musl binary (see Containerfile).
container:
    #!/usr/bin/env bash
    set -euo pipefail
    cd "$(git rev-parse --show-toplevel)"
    cargo build --release --target x86_64-unknown-linux-musl --no-default-features --features server,client,noise,hot-reload,multiplex,kcp,transparent
    mkdir -p img/bin/amd64
    cp target/x86_64-unknown-linux-musl/release/molehill img/bin/amd64/
    docker build -f Containerfile -t molehill img/
