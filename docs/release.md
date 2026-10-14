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

The **wire protocol** has its own number, and it moves with the release line:
it changes with the tag that introduces it, and no two tags are compatible. The
rule, the current dialect and the release line are in AGENTS.md §5; the wire
consequences are in [internals.md](internals.md), "Protocol versions".

## Release notes: CHANGELOG.md is the single source

`CHANGELOG.md` is maintained in
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) format.

- During development, record notable changes under `## [Unreleased]`.
- Before tagging, move that content into a dated section:
  `## [x.y.z] - YYYY-MM-DD` (the git tag is the same version with a `v`
  prefix, e.g. `v0.7.1`), leaving `## [Unreleased]` empty. The section must be
  the only one for that version and must contain prose, not just headings:
  every extractor takes the first match, so a stray duplicate in front of the
  real section would be published as the notes.
- A missing or empty changelog section **fails the release** — the workflow
  errors out before building anything. Fix: add the section, delete the tag,
  re-push. Never hand-edit release notes on GitHub.

## Tag-push policy: no casual release pushes

Commits are always allowed — the fast gates guard them and they trigger
nothing public. Pushing a `v*` tag is a deliberate release act; the rule and its
five preconditions are stated in [AGENTS.md §5](../AGENTS.md). The release
workflow enforces the version and changelog ones mechanically, and the local tag
review (`githooks/pre-tag`, next section) checks most of the others before the
tag exists. Re-tagging is allowed only to fix a failed release (delete the tag,
fix, re-push).

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
exactly one dated non-empty changelog section with an empty `[Unreleased]`,
the committed bench results/chart, and
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

The method — what is measured, how a metric is defined, the conditions and
timelines, the SLO, the scenarios, and how to reproduce a run — is owned by
[benchmarks.md](benchmarks.md); this section owns the ritual a tag depends on.
The ritual produces three artifacts and one verdict: the results file
(`benches/records/results-bench-vX.Y.Z.json`), the chart set in `assets/`, the
refreshed README benchmark numbers, and `just bench-gate`'s verdict on the run
(plus its comparison against the previous tag when a baseline is given).

The sweep is one profile of the one model, so the staged schedule and the load
ramp travel in the same artifact: `--profile sweep` runs the `timeline` and
`capacity` scenarios over the arms the profile names (`molehill`, `frp`,
`rathole`, `nps`). They are two instruments and never cross-check each other —
the schedule answers what happens as the path changes over time, the ramp
answers where the ceiling is — and every cell records the scenario and stage it
came from, so the gate compares like with like.

Peers are frp, rathole (upstream) and nps, each fetched as the **latest GitHub
release** binary, never built from source, with the resolved versions recorded
in the results meta. Every entry is a PEP 723 python script run via `uv run`,
and the whole package is self-contained (`benches/scripts/bench/`).

1. `just interop` — the interop matrix
   ([checks.md](checks.md#outside-the-chain-the-interop-matrix-just-interop)):
   this build against the previous release's binary. A wire-format change first
   meets a real old peer here, and nowhere else, so run it **before** the sweep
   — it is seconds. What it asserts is that both cross-version directions
   **refuse** each other on the connection it happens on, and that the refusing
   process keeps serving a peer of its own version.
2. `just bench-peers` — fetch/refresh the peer binaries (cached per release).
3. `just bench --profile sweep --binary target/release/molehill \
   --out benches/records/results-bench-vX.Y.Z.json`
   — run the sweep. **The profile's arm list is the requirement**: a run whose
   arms are not `molehill,frp,rathole,nps` produces a file that looks like a
   release artifact with an empty peer comparison in it, so name them
   explicitly if the profile ever changes. The release artifact path is passed
   explicitly: the default `--out` is `~/tmp/bench-<stamp>.json`, which is
   scratch and never the committed evidence. Before the run: the tree must be
   clean and the binary freshly built, because a number has to describe code
   someone can check out (AGENTS.md §10). The results meta records the revision
   and `tree_clean` separately — the revision names the commit, and
   `tree_clean` excludes the results file the run is writing, so an uncommitted
   *source* change is the only thing that makes it false.
   Budget: the sweep profile's timeline is ~17 minutes per arm plus the ramp,
   so four arms are about two hours on this host. `--scale 0.2` shortens the
   holds without changing the schedule's *shape*, and the fingerprint records
   the scale, so a shortened run can never be mistaken for the published one.
4. `just bench-plot benches/records/results-bench-vX.Y.Z.json --out-dir assets/`
   — renders the chart set and prints the markdown tables: the master (per
   arm, the interactive stream over the stage schedule with its per-stage
   p50/p99 and the bulk throughput, wedges marked), the small multiples, the
   capacity curve, the UDP ladder, the drift panels and the cost bars; the
   figures are written per results file, so the release renames/moves the ones
   the README embeds and deletes the previous tag's. Update the README
   Benchmarks section with them and their numbers.
   The release review enforces the provenance half of this step: the results
   file records the revision it was measured at, and `githooks/pre-tag` fails
   the tag if `src/`, `tests/`, `Cargo.*`, `build.rs` or the model changed
   between that revision and the reviewed commit — a chart whose numbers
   describe code that is no longer here is the one thing a reader cannot see.
   Doc and asset commits after the sweep are fine; that is how the ritual lands
   it.
5. `just bench-gate benches/records/results-bench-vX.Y.Z.json \
   --baseline benches/records/results-bench-v<previous>.json`
   — the gate: coverage (every cell the scenarios declared), the endpoint
   invariant, the SLO on the clean stages, the drift and wedge axes, the
   capacity ramp, and the regression half when a comparable baseline is given.
   It blocks the tag on a violation; a peer that misses the SLO is reported
   with its number and does not block the tag. The gate runs locally before
   tagging and never in CI (shared runners are too noisy for performance
   numbers); weak-network cells need `CAP_NET_ADMIN`. The method, the
   thresholds and the comparability rules are in
   [benchmarks.md](benchmarks.md#the-gate).

The soak model's records (`results-soak-vX.Y.Z.json`, also in
`benches/records/`) were measured with the model this one replaced. They stay
as published history and are never a regression baseline for a `bench` run:
the two schemas are different instruments, and `bench-gate` refuses a baseline
whose method record does not match.

## Comparing two builds (development screening)

Outside the release ritual, the question is usually *"is this direction worth
pursuing?"* — and the answer must be minutes, not hours. That is the `screen`
profile: one condition, one workload, two builds (or two configurations of one
build) measured in the same rotation, with a per-metric verdict that clears the
run's own noise floor:

```bash
# build both, then one interleaved A/B
just bench --profile screen --ab-arm molehill --binary-b /path/to/other/build \
     --out ~/tmp/bench-ab.json
just bench-compare ~/tmp/bench-ab.json ~/tmp/bench-ab-first.json
```

It is the fast, development-time form of
[benchmarks.md](benchmarks.md#running-it)'s two-build comparison — minutes
instead of a sweep, and the verdict says "no claim" when the difference is
inside the instrument's own scatter.

## What the release workflow does

1. **pre-release checks**: Cargo.toml version ↔ tag version, CHANGELOG
   section present, `cargo audit`.
2. **build matrix** (9 targets): linux gnu + musl, aarch64 musl (via cross),
   arm/armv7 musl (`embedded` feature), macOS x86_64 + aarch64, Windows
   msvc. musl artifacts build with the full feature set
   (`server,client,noise,hot-reload,multiplex`); linux artifacts are
   UPX-compressed. Tests run inside the matrix for native and cross targets.
3. **GitHub Release**: published **directly** (no draft stage; the tag push
   itself is the human checkpoint), with notes extracted from `CHANGELOG.md`,
   all archives, and a `SHA256SUMS` — never edit the notes by hand.
4. **GHCR**: publishes the multi-arch scratch image
   (`ghcr.io/niyueee/molehill:<tag>` and `:latest`) from the musl artifacts,
   then smoke-tests it.
5. **crates.io**: publishes `molehill-rathole` using the `CRATES_IO_API_TOKEN`
   secret.

## CD test builds: per-commit, per-platform artifacts

`.github/workflows/test-build.yml` builds **test artifacts** from any commit
without creating a release: dispatch it manually from the Actions tab, choose
a `ref` (commit SHA, branch, or tag) and `targets` (`linux`, `macos`,
`windows`). Artifacts are ephemeral (7-day retention) and are never a Release;
the usage rules and typical uses are in [AGENTS.md §6](../AGENTS.md).
