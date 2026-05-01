# Justfile for molehill

set positional-arguments

# Run all checks: format, type check, lint, audit, outdated deps, unused deps
# 前置: cargo install cargo-audit && cargo install cargo-machete && cargo install cargo-outdated
check:
  cargo fmt --check
  cargo check
  cargo clippy -- -D warnings
  cargo audit
  cargo outdated --root-deps-only
  cargo machete
