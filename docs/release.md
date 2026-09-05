# Releases

> English | [简体中文](release.zh.md)

Releases are **tag-driven**. The only trigger of a release is pushing a `v*`
tag; `.github/workflows/release.yml` owns the whole flow and no other path
publishes a release.

## Versioning

molehill is a fork of [rathole](https://github.com/rapiz1/rathole) and counts
its version numbers **from the upstream line**: upstream's last release was
v0.5.0 and the fork continued at v0.6.0. [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
applies within that line.

## Release notes: CHANGELOG.md is the single source

`CHANGELOG.md` is maintained in
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) format.

- During development, record notable changes under `## [Unreleased]`.
- Before tagging, move that content into a dated section:
  `## [x.y.z] - YYYY-MM-DD` (the git tag is the same version with a `v`
  prefix, e.g. `v0.7.1`).
- A missing or empty changelog section **fails the release** — the workflow
  errors out before building anything. Fix: add the section, delete the tag,
  re-push. Never hand-edit release notes on GitHub.

## Tag-push policy: no casual release pushes

Commits are always allowed — the fast gates guard them and they trigger
nothing public. Pushing a `v*` tag is a deliberate release act; the four
preconditions (explicit human request, `Cargo.toml` version match, dated
changelog section, green `just check`) are the repository rule stated in
[AGENTS.md §5](../AGENTS.md) — the release workflow enforces the version and
changelog ones mechanically.

Re-tagging is allowed only to fix a failed release (delete the tag, fix,
re-push). For verifying a commit without releasing, use CD test builds.

## What the release workflow does

1. **pre-release checks**: Cargo.toml version ↔ tag version, CHANGELOG
   section present, `cargo audit`.
2. **build matrix** (9 targets): linux gnu + musl, aarch64 musl (via cross),
   arm/armv7 musl (`embedded` feature), macOS x86_64 + aarch64, Windows
   msvc. musl artifacts build with the full rustls feature set; linux
   artifacts are UPX-compressed. Tests run inside the matrix for native and
   cross targets.
3. **GitHub Release**: created as a **draft**, with notes extracted from
   `CHANGELOG.md`, all archives, and a `SHA256SUMS`. Publish after a quick
   look — never edit the notes by hand.
4. **GHCR**: publishes the multi-arch scratch image
   (`ghcr.io/niyueee/molehill:<tag>` and `:latest`) from the musl artifacts,
   then smoke-tests it.
5. **crates.io**: publishes `molehill-rathole` using the `CRATES_IO_API_TOKEN`
   secret.

## CD test builds: per-commit, per-platform artifacts

`.github/workflows/test-build.yml` builds **test artifacts** from any commit
without creating a release: dispatch it manually from the Actions tab, choose
a `ref` (commit SHA, branch, or tag) and `targets` (`linux`, `macos`,
`windows`).

- Artifacts are ephemeral (7-day retention) and are never a Release — do not
  hand out release links for them, and do not reference them in the
  changelog.
- Typical uses: verifying that a specific commit compiles on all platforms
  before tagging, and reproducing platform-specific issues on an exact
  commit.
