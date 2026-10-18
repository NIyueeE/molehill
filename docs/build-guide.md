# Build Guide

This page is for contributors building `molehill` from source — for the latest
features, another platform, or a smaller binary.

## Build

To use default build settings, run:

```sh
cargo build --release
```

## Customize the Build

The crate is feature-gated: `[features]` in [Cargo.toml](../Cargo.toml) is the
list of what can be compiled in or out.

For example, to build `molehill` with the `client` and `noise` feature:

```sh
cargo build --release --no-default-features --features client,noise
```

(Note that the default feature set is `server`, `client`, `noise`,
`hot-reload`, `multiplex`, `kcp` and `transparent`; `transparent` needs a TUN
device — Linux and macOS run both ends, Windows can serve but not claim — and
pulls the `tun-rs` dependency. `transparent` implies `carrier`, which owns the
KCP/TCP choice, fallback and lane budget, and `noise`, because the control
channel that carries its registration is always Noise. A claim's own traffic
is one connection per lane and never yamux. `multiplex` also implies `carrier`
for the tunnel pool, and `kcp` implies `multiplex` (a KCP tunnel is a mux
tunnel).
A Windows server also needs `wintun.dll` at runtime.)

The feature graph is additive:

- `carrier` — the data-plane carrier choice and fallback (KCP where `kcp` is
  enabled, else TCP), independent of yamux. `multiplex` and `transparent` imply
  it; `transparent` also implies `noise` for its control channel.
- `multiplex` — the yamux tunnel pool; implies `carrier`.
- `kcp` — KCP-over-UDP tunnels; implies `multiplex`.

Dependency features are scoped to the feature that uses them: `noise` enables
`snow`, `chacha20poly1305`, `base64` and `pin-project`; `kcp` enables `log` and
`thiserror`; `client` enables the proxy and status dependencies; `multiplex`
enables `parking_lot` and `nohash-hasher`. Tokio is built with only
`rt-multi-thread`, `macros`, `net`, `io-util`, `sync`, `time`, `signal` and
`fs`; an API from another Tokio feature needs that feature added here.

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

### Platform conditional compilation

The platform predicates live in `build.rs`, not at each call site. It emits
the target OS aliases (`mh_linux`, `mh_macos`, `mh_windows`) and the L3 aliases
(`mh_l3`, `mh_l3_client`, `mh_l3_server`), combining `TARGET` with the enabled
features. L3 code must use those aliases instead of repeating
`all(feature = ..., any(target_os = ...))`; adding a platform or changing an
end's support means changing `build.rs` once.

The same script emits `native_target` only when host and target share both
architecture and OS family (Linux gnu/musl counts as one family). Tests that
spawn the built binary gate on it, so a cross-OS binary is never treated as
executable.

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

Measured on `x86_64-unknown-linux-gnu` with this tree (2026-10-18): the
`--features client` build is **1 927 048 bytes** (1.84 MiB) and the
`--features embedded` build is **2 676 424 bytes** (2.55 MiB), both already
stripped. The Linux release archives are UPX-compressed, so their published
size is the packed figure; `just container` builds the musl image binary
locally.
