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

The method — what is measured, how to read the charts, the stage schedule,
the SLO, the test types, and how to reproduce a run — is owned by
[benchmarks.md](benchmarks.md); this section owns the ritual a tag depends on.
The ritual produces three artifacts and one verdict — the results file
(`benches/scripts/soak/results-soak-vX.Y.Z.json`), the chart set in `assets/`
and the refreshed README benchmark numbers, then `soak-check`'s verdict on the
run and its comparison against the previous tag. **The results file carries the
load axis as well as the staged schedule**: the sweep runs
`--test=rrul,capacity`, so "how much can it carry before the SLO breaks" travels
in the same artifact, measured on the same host at the same revision. The two
are separate instruments and are never cross-checked — the schedule answers what
happens as the path changes over time, the ramp answers where the ceiling is —
and the file keys each entry by (tool, test type), so the gate compares like
with like.

The tools live in `benches/scripts/soak/`; peers are frp, rathole (upstream)
and nps, each fetched as the **latest GitHub release** binary, never built
from source, with the resolved versions recorded in the results meta. All
entries are PEP 723 python scripts run via `uv run`; a run refuses to start
while another one holds the lock, and a killed run (Ctrl-C or SIGTERM) still
writes the tests it completed.

1. `just interop` — the interop matrix ([checks.md](checks.md#outside-the-chain-the-interop-matrix-just-interop)):
   this build against the previous release's binary. A wire-format change first
   meets a real old peer here, and nowhere else, so run it **before** the sweep
   — it is seconds. What it asserts is that both cross-version directions
   **refuse** each other on the connection it happens on, and that the refusing
   process keeps serving a peer of its own version. For a v4-only release that
   refusal *is* the expected result: a matrix that starts forwarding across
   releases, or one whose refusal is not local, is what means the release is not
   ready.
2. `just soak-peers` — fetch/refresh the peer binaries (cached per release).
3. `just soak --test=rrul,capacity --tools molehill,frp,rathole,nps
   --out benches/scripts/soak/results-soak-vX.Y.Z.json`
   — run the sweep (one tool or a batch of them, per its own shaped path in
   one HTB class each, so concurrent tools never share a shaper; the batch
   size comes from the host's CPU budget). **`--tools` is required**: its
   default is `molehill` alone, which produces a file that looks exactly like
   a release artifact but has an empty peer comparison in it — every tool the
   README's table names has to be in the run. **Both test types are required**:
   `--test` defaults to `capacity` alone, so a run without `rrul` has no staged
   schedule, and a run without `capacity` publishes no load axis at all — both
   write a file that looks like a release artifact. The release artifact path is passed
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
   The release review enforces the provenance half of this step: the results
   file records the revision it was measured at, and `githooks/pre-tag` fails
   the tag if `src/`, `tests/`, `Cargo.*`, `build.rs` or the runner changed
   between that revision and the reviewed commit — a chart whose numbers
   describe code that is no longer here is the one thing a reader cannot see.
   Doc and asset commits after the sweep are fine; that is how the ritual lands
   it.
5. `just soak-check` — the gate, in two steps. First the run is checked
   against itself: every coverage axis a test claims must have carried
   samples, **every stage that claims a bulk spine must have carried intervals
   inside its own window** (a stage whose probe never connected used to pass on
   the other stages' sample count), every throughput sample must have dialed
   the tool's exposed port rather than its backend, every stage that carried a
   spine states what it carried or why it cannot (a rate stage's defeated
   sender accounting is reported, not published as a number), and the released
   tool must meet the absolute SLO
   **on the unshaped clean stages** (a saturated `rrul`/`soak` stage is above
   the SLO by design — that is the degradation curve, reported as a note, not
   judged). The SLO gates the tool this repository releases; a peer that
   misses it is reported with its number and does not block the tag. Without
   a baseline that self-check *is* the verdict, and that is how the first
   Soak release (v0.9.0) is gated: the absolute SLO on the clean stages plus
   the run's own completeness and endpoint checks. With the previous tag's
   file it then compares per test type: **a tool must not lose capacity, must
   not break its SLO earlier, must not wedge where it did not and must not
   drift**; a violation blocks the tag until fixed or explicitly waived.
   Per-stage p99 difference verdicts apply to the **unshaped** control stages:
   a netem stage's number is dominated by the queue the harness installed and
   one run does not repeat it (measured: 5-24 % across three runs of one
   unchanged method), so the gate reports it as context and fails only a
   blow-up — see benchmarks.md, "Comparability".
   (record the waiver in `HANDOFF.md`). Only same-schema, same-method, same-host
   runs are comparable, so a baseline from another host, another
   `workload_version` **or another instrument** is not a gate input: the gate
   compares the runs' method records — the schedule, the shaper classes, the
   load, the SLO, the probe rates and the stage-transition settings
   (`soak_check.METHOD_KEYS`) — and refuses the comparison naming every key
   that differs, and every key one file does not record at all. A single
   `workload_version` integer cannot carry that on its own: the v0.10.0 cycle
   changed five of those keys while it stayed `1`. A run that predates a field
   the gate needs (the
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

The same interleave compares **two configurations of one build** when the
question is a configuration decision rather than a code change
(`--ab-variants mux,direct`): the arms then differ by config alone, which is
what makes a placement or mode question answerable without a build axis riding
along. The two modes and the slow-visitor probe that makes head-of-line
blocking measurable are documented in
[benchmarks.md](benchmarks.md#the-slow-visitor-soak_slow_visitor_bps).

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
