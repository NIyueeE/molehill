# Justfile for rathole

set positional-arguments

# Run all checks: type check, lint, format, audit, unused deps
# Prerequisites: cargo install cargo-audit && cargo install cargo-machete
check:
  cargo check
  cargo clippy -- -D warnings
  cargo fmt --check
  cargo audit
  cargo machete
