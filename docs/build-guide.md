# Build Guide

This is for those who want to build `molehill` themselves, possibly because the need of latest features or the minimal binary size.

## Build

To use default build settings, run:

```sh
cargo build --release
```

## Customize the Build

`molehill` comes with lots of *crate features* that determine whether a certain feature will be compiled or not. Supported features can be checked out in `[features]` of [Cargo.toml](../Cargo.toml).

For example, to build `molehill` with the `client` and `noise` feature:

```sh
cargo build --release --no-default-features --features client,noise
```

(Note that the default feature set is `server`, `client`, `noise`,
`hot-reload`, `multiplex`, `kcp` and `transparent`; `transparent` needs Linux
and a TUN device.)

Two names exist outside the default set:

- `embedded` — the embedded-device release set: `server`, `client`,
  `hot-reload` and `noise`, without `multiplex`, `kcp` or `transparent`;
  the release workflow builds it for the embedded targets.
- `console` — tokio-console instrumentation; a debugging aid, disabled by
  default, not for production.

Every feature name in `[features]` is public API — `--features <name>` is
valid for each — and the table is kept in sync with this page by the docs
gate: a name documented nowhere is either a dead pipeline row or an
undiscoverable feature, and both fail `githooks/check-docs`.

## Minimalize the binary

1. Build with the `minimal` profile

The `release` build profile optimizes for the program's running time, not the binary size.

However, the `minimal` profile enables lots of optimization for the binary size to produce a much smaller binary.

For example, to build `molehill` with `client` feature with the `minimal` profile:

```sh
cargo build --profile minimal --no-default-features --features client
```

2. `strip` and `upx` (optional)

`[profile.minimal]` inherits `[profile.release]`, which sets `strip = true`,
so step 1 already produces a stripped binary — `strip` has nothing left to
remove. `upx` can still compress it:

```sh
upx --best --lzma molehill
```

Measured on `x86_64-unknown-linux-gnu` with this tree (2026-10-17): the
`--features client` build is **1 927 808 bytes** (1.84 MiB) and the
`--features embedded` build is **2 675 176 bytes** (2.55 MiB), both already
stripped. The Linux release archives are UPX-compressed, so their published
size is the packed figure; `just container` builds the musl image binary
locally.
