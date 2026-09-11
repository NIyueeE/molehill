# Releases


Releases are **tag-driven**. The only trigger of a release is pushing a `v*`
tag; `.github/workflows/release.yml` owns the whole flow and no other path
publishes a release.

## Versioning

molehill's version line began at v0.6.0 — the point where it forked from
[rathole](https://github.com/rapiz1/rathole); upstream's last release was
v0.5.0 — and has been numbered independently since.
[Semantic Versioning](https://semver.org/spec/v2.0.0.html) applies within
that line.

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
nothing public. Pushing a `v*` tag is a deliberate release act; the five
preconditions (explicit human request, `Cargo.toml` version match, dated
changelog section, green `just check`, green benchmark gate — see below) are
the repository rule stated in [AGENTS.md §5](../AGENTS.md) — the release
workflow enforces the version and changelog ones mechanically, and the local
tag review (`githooks/pre-tag`, next section) checks most of them before the
tag exists.

Re-tagging is allowed only to fix a failed release (delete the tag, fix,
re-push). For verifying a commit without releasing, use CD test builds.

## Tag review: `githooks/pre-tag`

Releases are published **directly** — there is no draft stage — so the
human checkpoint is the tag push itself, and a light release review runs at
the two tag moments (responsibilities of the three hooks are split in
[checks.md](checks.md)):

- **Locally**: `just tag` runs the review against HEAD, then creates the
  annotated tag for `Cargo.toml`'s version. `just tag-check` runs only the
  review.
- **On tag push**: git has no native tag hook, so `githooks/pre-push` re-runs
  the review (against the tag's commit) for every `v*` tag in the push,
  before the heavy gates — a violation fails the push.

The review mechanically verifies tag↔version match (including `Cargo.lock`),
a dated non-empty changelog section, the committed bench results/chart, and
the container job structure (GHCR job, image tags, `--help` smoke test),
then prints an advisory checklist — CHANGELOG and docs audit, container
build review, benchmark gate, deliberate-release confirmation — that only a
human/agent review can clear. Bench assets are required at tag creation; on
a re-push of a historical tag they degrade to a note (the ritual postdates
older tags, and re-pushing one to fix a failed release must stay possible).
It is deliberately light: heavy gates
(audit/deny/outdated/test) belong to pre-push and CI, and release.yml
re-enforces the version and changelog invariants remotely.

## Benchmarks: per-tag ritual

Every tag refreshes the peer-comparison benchmark (matrix design and peer
fetching live in `benches/scripts/bench/`; peers: frp, rathole (upstream),
bore — each fetched as the **latest GitHub release** binary, never
built from source, with the resolved versions recorded in the results meta
and the chart footer). The matrix (schema v3) measures, per tool and network
cell: TCP
throughput (1/8 streams) **through the tunnel** (iperf3 dials the tool's
exposed port), connection-path RTT, data-path RTT (steady ping over
one established connection), UDP session quality over one established session
(RTT / loss / jitter / max inter-packet gap), a head-of-line probe (saturating
bulk flow + game-like pinger through the same tunnel), and RSS. Molehill runs
as mux, noise, mux1 and kcp4 variants (mux-off additionally on the loopback
cell); peers: frp / rathole also run UDP arms, bore is TCP-only, and all
three run a lean cell subset (loopback, rtt10, 1% loss, the two rate cells)
— the peers chart plots only those cells. All bench
entries are PEP 723 python scripts run via `uv run`: runs are resumable (each
completed arm is checkpointed together with the full meta), continue on
error (a per-metric failure records `null` plus a `partial_metrics` list
instead of a fake 0; a probe that is structurally out of range — the
64-stream scale point above an arm's `count × 32` yamux ceiling — is
skipped by design with its reason recorded the same way; the rate20
8-stream test wedges the single-test iperf3 server and records the timeout
instead), refuse to run concurrently (a global lock — concurrent
runs used to reap each other's live processes), and a killed run (Ctrl-C or
SIGTERM) cleans up arms, removes the netem qdisc and writes the full meta;
`--fresh` backs up the previous results file to `.bak` first.
`--tools/--cells/--variants` select subsets — the full matrix is roughly
three hours at full rigor:

1. `just bench` — runs the full matrix (loopback + weak-network cells) and
   writes `benches/scripts/bench/results-vX.Y.Z.json` (the default `--out`
   derives from `Cargo.toml`'s version, so a plain run targets the next
   tag's file, never the previous release's baseline). During development,
   `just bench-fast` runs a ~2-minute molehill-only smoke matrix into
   `results-dev.json`, which plot/regression never pick up.
   **The full matrix is a heavy, machine-exclusive ritual (~1.5-2 h)**:
   it saturates every reachable core by design. It self-throttles (nice
   10, load-aware cooldown between arms) so the host stays responsive,
   but do not run other work on the same machine while it runs.
2. `just bench-plot` — renders the per-axis chart set
   (`assets/benchmark-vX.Y.Z.png` for the peers baseline,
   `benchmark-mux-*`, `benchmark-transport-*` for encryption,
   `benchmark-count-*` and `benchmark-carrier-*` — one single-variable
   comparison each) and prints the markdown tables; update the README
   Benchmarks section with them, then delete the previous tag's charts from
   `assets/`.
3. `just bench-check` — regression gate against the previous tag's results
   file. **Performance must not regress vs the previous tag**; a violation
   blocks the tag until fixed or explicitly waived (record the waiver in
   `HANDOFF.md`).
4. Commit results JSON + new chart + README table **in the release commit**.
5. `just tag` — the release review (`githooks/pre-tag`) must pass, then the
   annotated tag for `Cargo.toml`'s version is created locally. Pushing it
   (`git push origin vX.Y.Z`) is the release act: pre-push re-runs the review
   and the heavy gates, then release.yml publishes directly.

The gate runs locally before tagging, never in CI: shared runners are too
noisy for performance numbers. Weak-network loss cells need `CAP_NET_ADMIN`
(netem); without it the script falls back to a userspace delay proxy for
rtt cells and skips loss cells — note the mechanism in the README table when
it differs. The gate is only meaningful between same-schema AND same-method
results: schema v3 fixed the throughput measurement point (pre-v3 files
dialed the backend directly and are not comparable), and the 2026-09-10
measurement revision changed the rate-cell shaping queue depth and the
throughput window convention, so the v0.8.0 **rate-cell** rows are not
comparable to later runs (the other cells are). A rate-cell-only difference
against the v0.8.0 baseline is therefore not a regression signal; record how
the gate was read in `HANDOFF.md`. `benches/scripts/bench/audit_results.py`
is the companion check for completeness — it reports `None` holes and
arm-level errors, and exits non-zero when it finds either.

## What the release workflow does

1. **pre-release checks**: Cargo.toml version ↔ tag version, CHANGELOG
   section present, `cargo audit`.
2. **build matrix** (9 targets): linux gnu + musl, aarch64 musl (via cross),
   arm/armv7 musl (`embedded` feature), macOS x86_64 + aarch64, Windows
   msvc. musl artifacts build with the full feature set
   (`server,client,noise,hot-reload,multiplex`); linux artifacts are
   UPX-compressed. Tests run inside the matrix for native and cross targets.
3. **GitHub Release**: published **directly** (no draft stage — the human
   checkpoint is the tag push itself, guarded locally by the pre-tag
   review), with notes extracted from `CHANGELOG.md`, all archives, and a
   `SHA256SUMS`. Never edit the notes by hand.
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
