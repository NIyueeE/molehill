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

The method — what is measured, how to read the charts, the stage schedule,
the SLO, the test types, and how to reproduce a run — is owned by
[benchmarks.md](benchmarks.md); this section owns the ritual a tag depends on.
The ritual produces three artifacts and one verdict — the results file
(`benches/scripts/soak/results-soak-vX.Y.Z.json`), the chart set in `assets/`
and the refreshed README benchmark numbers, then `soak-check`'s verdict on the
run and its comparison against the previous tag.

The tools live in `benches/scripts/soak/`; peers are frp, rathole (upstream)
and nps, each fetched as the **latest GitHub release** binary, never built
from source, with the resolved versions recorded in the results meta. All
entries are PEP 723 python scripts run via `uv run`; a run refuses to start
while another one holds the lock, and a killed run (Ctrl-C or SIGTERM) still
writes the tests it completed.

1. `just interop` — the interop matrix ([checks.md](checks.md#outside-the-chain-the-interop-matrix-just-interop)):
   this build against the previous release's binary. A wire-format change first
   meets a real old peer here, and nowhere else, so run it **before** the sweep
   — it is seconds, and a rejection means the release is not ready.
2. `just soak-peers` — fetch/refresh the peer binaries (cached per release).
3. `just soak --test=rrul --out benches/scripts/soak/results-soak-vX.Y.Z.json`
   — run the sweep (one tool or a batch of them, per its own shaped path in
   one HTB class each, so concurrent tools never share a shaper; the batch
   size comes from the host's CPU budget). **`--test=rrul` is required**: the
   default test type is `capacity`, which produces a ceiling probe rather than
   the staged release sweep, and the file it writes looks like a release
   artifact. The release artifact path is passed
   explicitly: the default `--out` is `results-soak-dev.json` beside the
   script, which `soak-plot`/`soak-check` do read but which is never the
   committed evidence. The run covers the test types the release needs
   (`--test`; see [benchmarks.md](benchmarks.md#test-types) for what each one
   answers). Before the run: the tree must be clean and the binary
   freshly built, and the results meta records the revision and the binary
   version, because a number has to describe code someone can check out
   (AGENTS.md §10).
   The meta records `revision` and `tree_clean` separately: the revision names
   the commit, and `tree_clean` excludes the results file the run is writing
   (producing an artifact must not be what marks it dirty), so an uncommitted
   *source* change is the only thing that makes it false.
4. `just soak-plot` — renders the chart set and prints the markdown tables:
   `assets/soak-vX.Y.Z.png` (the master: per tool, the interactive stream over
   the stage schedule with its per-stage p50/p99 and the bulk throughput,
   wedges marked), `-stages.png` (small multiples, one panel per stage),
   `-capacity.png` (the response-time-vs-load curve, only when the run had a
   capacity test), `-udp.png` (RTT plus the sliding loss rate), `-drift.png`
   (the fitted slopes) and `-cost.png` (only for a `cost` run). Update the
   README Benchmarks section with them and their numbers, then delete the
   previous tag's charts from `assets/`.
5. `just soak-check` — the gate, in two steps. First the run is checked
   against itself: every coverage axis a test claims must have carried
   samples, every throughput sample must have dialed the tool's exposed port
   rather than its backend, and the released tool must meet the absolute SLO
   **on the unshaped clean stages** (a saturated `rrul`/`soak` stage is above
   the SLO by design — that is the degradation curve, reported as a note, not
   judged). The SLO gates the tool this repository releases; a peer that
   misses it is reported with its number and does not block the tag. Without
   a baseline that self-check *is* the verdict, and that is how the first
   Soak release (v0.9.0) is gated: the absolute SLO on the clean stages plus
   the run's own completeness and endpoint checks. With the previous tag's
   file it then compares per test type: **a tool must not lose capacity, must
   not break its SLO earlier, must not wedge where it did not and must not
   drift**; a violation blocks the tag until fixed or explicitly waived
   (record the waiver in `HANDOFF.md`). Only same-schema, same-host runs are
   comparable, so a baseline from another host or another `workload_version`
   is not a gate input. A run that predates a field the gate needs (the
   endpoint record, the revision) is reported as `LEGACY`: neither a pass nor
   a violation — the gate names what it could not verify, and the count of
   those checks is printed in the summary.

The gate runs locally before tagging, never in CI: shared runners are too
noisy for performance numbers. Weak-network loss cells need `CAP_NET_ADMIN`
(netem); without it the run aborts — there is no userspace fallback, because
a fallback path is a second measurement method. The gate is only meaningful
between same-model results: the retired matrix's numbers (v0.8.x and
earlier, in git history — its runner, charts and result files are no longer
in the tree) measured cold cells with medians over reps, so they are a
different instrument and never a regression signal against this model.
`benches/scripts/soak/soak_check.py` is the companion that applies the
run's self-check, the per-type threshold rules and the screen verdict.

### Comparing two builds (development screening)

Outside the release ritual, the question is usually *"is this direction
worth pursuing?"* — and the answer must be minutes, not hours. That is the
`screen` test type: one test type, one path class, one configuration pair,
the two builds **interleaved inside every load step** (the pair runs in the
same batch, so both sample the same machine state — sequential
before/after runs are defeated by epoch drift), with a sequential decision:

```bash
# build both, then one interleaved screen run
just soak --test=screen --path=loss1 --streams-max=8 \
     --ab /path/to/bin-parent,/path/to/bin-head --out results-screen.json
just soak-check --screen results-screen.json    # per-step verdict
```

The run and the verdict are the fast, development-time form of
[benchmarks.md](benchmarks.md#reproduce-it-yourself)'s two-build comparison —
minutes instead of a sweep.

The verdict prints per-step values for both builds, the effect size, and a
CLAIM only where **every** step favours the same build by more than the
threshold (in either direction: a claim against the change is as much a
verdict as one for it); anything else is reported as *directional*. A screen
verdict is **domain-scoped**: it says whether to pursue the direction on
that path class, never whether the change may ship — the sweep and the gate
decide that. The retired matrix's `--ab` mode did the same job for the old
model; the screen is its successor and adds the SLO instrument as the second
measured axis.

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
