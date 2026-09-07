# Audit verification ledger

Date: 2026-09-07. This ledger covers the current uncommitted audit fixes.
“Regression passed” means a targeted correctness check passed; it does not imply
exhaustive correctness, a completed feature matrix, or measured performance.
Source-fix evidence below is supplied by the implementing agent unless a command
is listed in the verification section. The five-configuration local correctness matrix is complete. No commit or remote CI run was made. The final demo ETag/If-Match integration fix also passes; the measured library workload was unaffected by that example-only change.

| Audit item | Current status | Evidence / remaining limit |
| --- | --- | --- |
| 2.1 Pixel-center offset | Regression passed | Half-pixel coordinates and crop eligibility regressions failed before fixes and passed afterward; local feature matrix passed. |
| 2.2 Affine transforms and source window | Regression passed | Four-corner window bounds and flip/shear fast-path gating; local feature matrix passed. |
| 2.3 Projected bounds | Regression passed | Polar and partially valid projection boundary regressions; not a proof for every CRS/domain. |
| 2.4 PixelIsPoint | Regression passed | Tiepoint and transformation-matrix georeferencing regressions failed before the fix and passed afterward. |
| 2.5 Resampling kernels | Regression passed, nonlinear approximation remains | Scalar cubic, B-spline, Lanczos, one-pixel average and nearest-upsample cases. Average clips the projected four-corner polygon against source cells, including rotation/shear. Nonlinear pixel edges still use straight segments and projection-grid interpolation remains approximate. |
| 3.1 Nodata interpolation / style override | Regression passed | Kernel-support validity, nonfinite-neighbor exclusion, pre-interpolation overrides and mask-aware fallback/expression cases pass. |
| 3.2 Mosaic numeric nodata | Regression passed | Eleven reducer tests pass, including finite and NaN nodata exclusion across five reducers. |
| 3.3 Statistics NaN, mask and variance | Regression passed | NaN filtering, mask handling and Welford variance regressions. |
| 3.4 Fallback unscale | Regression passed | Fallback scale/offset regression. |
| 3.5 Single-source / mosaic style equivalence | Regression passed | Exact byte equality for three fixtures × eight single-asset style queries, including stretch, unscale, expression, colormap and nodata. Multi-asset sources unscale before reduction and use composite statistics. |
| 4.1 Overview tolerance | Regression passed | Six overview-selection tests pass, including degree-scale values (`0.0005` selects level 2). |
| 4.2 Work memory bounds | Guard regressions and bounded workload passed | Shared active-work budget (default 512 MiB), metadata/response ownership, window/block/compressed-byte guards, metadata preflight and bounded default decoders. Early rejection workload passes. Completed outputs and cache residency have separate ownership; this is not an OS RSS ceiling. See runtime contract. |
| 4.3 CPU work on async threads | Regression and responsiveness checks passed | Planning, statistics, rendering, DNS and TLS/client construction are bounded/offloaded. The workload caught synchronous client setup delaying deadlines; after correction, 10 ms deadline cases returned in approximately 12 ms. No real-time scheduling guarantee. |
| 4.4 I/O / request backpressure | Regression and bounded workload passed | Foreground read limit defaults to eight; shared request admission defaults to 64 and fails fast. Controlled eight-request workload with admission two produces six overload errors and two admitted terminations. |
| 4.5 Cache claim race / prefetch priority | Regression passed | Stale-miss race reproduced and fixed. Prefetch uses immediate read-budget acquisition instead of queuing; the contention regression passes. No sustained contention benchmark is claimed. |
| 4.6 Cancellation / deadlines | Cooperative contract verified | Default 30-second deadline plus per-call timeout/cancellation handle. Permits/accounting remain with active blocking work. Explicit cancellation, task abort and deadline workloads observe no new GETs/decodes after return during 250 ms settling. Running CPU instructions and remote handlers are not forcibly preempted. |
| 5.1 Signed URLs | Regression passed | Live HTTP tests preserve signed query strings and redact secrets from direct-open errors, post-open range failures, and captured prefetch error logs. The post-open exposure was reproduced before the fix. An earlier socket-blocked attempt is not counted as negative evidence. |
| 5.2 SSRF | Regression passed; policy limits remain | Redirect bypass reproduced outside sandbox and fixed; DNS failure closes access. Transport pins validated DNS results and disables proxies/redirects. Broader DNS/network policy behavior is not a production penetration test. |
| 5.3 Partial results | Regression passed | Failed assets propagate errors instead of returning no tile, fanout overflow errors instead of truncation, and zero chunk size errors. First reduction stops only when complete. |
| 5.4 Source version identity | Snapshot regressions passed | Managed opens require a strong ETag or version, pin conditional reads, and include validator/size in cache identity. Explicit mosaic invalidation refreshes the next reader. Existing readers remain pinned; consistency relies on truthful validators and backend atomic writes. |
| 5.5 Input validation | Regression passed | Invalid styles are rejected before source lookup/planning/reads; zero chunks, clock-range overflow and cache-size arithmetic overflow are rejected. Semaphore capacities are clamped safely. |
| 6 Wide integer precision and output dtype | Regression passed | Wide I32/U32/F64 remain on the F64 scalar path. Scalar results for U8/U16/F32 return to their native dtype, preserving default display styling; dtype-preservation regression failed before the fix and passed afterward. |
| 6 IFD classification | Regression passed | Unrelated dimensions, bands, reduced-image flags and aspect-ratio cases. |
| 6 Embedded statistics provenance | Regression passed | Inferred embedded fields are labeled Embedded, not Exact; regression failed before the fix and passed afterward. |
| 6 Without PROJ | Regression passed | Full no-default and mosaic-only configurations pass. The projected point test is feature-gated; arbitrary CRS transformations still require PROJ. |
| 6 Mutable compiled LUT | Regression passed | Compiled fields are crate-private with read-only getters; a passing compile-fail doctest prevents external mutation. |
| 7 Duplicate compilation / resize / style validation | Implemented; local feature matrix passed | Style compiles before I/O; multi-asset output uses one final render; nearest resize uses a shared executor. |
| 8 CI and required fixtures | Implemented; CI run pending | Push/PR feature matrix executes tests, checks, formatting and doctests. Four generated TIFFs are checked in. Required-fixture silent returns removed; two large local-image tests remain explicitly ignored. |
| 8 Performance and load matrix | Bounded synthetic measurements recorded | Optimized loopback runs at concurrency 1/4/8 record P50/P95/P99, RSS, GET/cache counters, cancellation/deadline outcomes and memory rejection. These are library workloads on tiny fixtures, not production capacity or an end-to-end HTTP tile-server load test. |

## Final local verification

Every row ran both `cargo test --locked --all-targets <flags>` and
`cargo test --locked --doc <flags>` outside the sandbox so local HTTP servers
could bind. All commands exited successfully. Counts below are passing tests;
each library run also reports two explicitly ignored optional local-image tests.

| Flags | Library | Fixture integration | Example tests | Doctests | Criterion test-mode cases |
| --- | ---: | ---: | ---: | ---: | ---: |
| Default | 345 | 1 | 12 | 4 | 6 |
| `--no-default-features` | 287 | 1 | 0 | 3 | 6 |
| `--no-default-features --features mosaic` | 338 | 1 | 12 | 4 | 6 |
| `--features stac,webp,jpeg` | 351 | 1 | 15 | 4 | 6 |
| `--all-features` | 351 | 1 | 16 | 4 | 6 |

The Criterion cases printed `Success` in test mode. No timing benchmark or
throughput claim follows from these runs.

Additional checks:

- The compiled default library test binary ran from a temporary directory
  containing only copied `tests/fixtures`, with no `data/` directory:
  **345 passed, zero failed, two ignored**, including signed-query transport
  preservation, HTTP error redaction, snapshot and resource-control regressions.
- All four TIFFs reproduce byte for byte using the Python standard-library
  generator. No fixture setup or download is needed for normal tests.
- Known-pixel assertions cover both overview levels, UInt8/UInt16 samples,
  nodata metadata, and a window crossing four source blocks.
- `cargo fmt --all -- --check` and `git diff --check` pass.
- Required-fixture silent returns are gone. The two optional JL1 and SVN3 cases
  have explicit ignore reasons.
- Fresh GitNexus indexing recorded 2177 nodes and 6569 edges. Change analysis
  reported 371 symbols and 167 affected flows across 26 tracked files, with
  critical risk from shared execution paths. Flow analysis hit its cap and
  dropped some paths; these are observed counts, not an exhaustive proof.
  The feature matrix and review do not prove the absence of regressions.
  New untracked fixture, harness, CI and documentation files are additional scope.

Earlier failing runs were used to validate regressions and diagnose integration
errors; they are superseded by the final matrix above. An initial HTTP attempt
was blocked by sandbox socket restrictions and is not evidence of product failure.

## Limits

### Viewer follow-up

The live demo exposed gaps not covered by the earlier matrix: a one-band
dataset inherited the viewers' RGB band preset, and raster sources started
before dataset metadata arrived. All three viewers now default to automatic
bands, normalize the incompatible legacy RGB preset, and defer tile requests
until selection/metadata are ready. `node tests/viewer_bootstrap.cjs` tests
the delayed-startup ordering and runs in CI. The single-band HTTP viewport
regression also passes.

Real Zstd imagery exposed an overly strict streaming history-window limit.
The decoder now uses single-pass decompression into a fixed-size output slice;
legal large-window frames load while oversized output remains rejected.
The new legal-frame regression and the complete all-feature library run pass
(352 passed, two explicit local-data ignores). These are follow-up checks;
the earlier load measurements and feature-matrix table describe the prior
snapshot, rather than measurements of this follow-up.

New resource and consistency contracts are documented in
[runtime-limits.md](runtime-limits.md). The bounded local measurement procedure is
in [load-audit.md](load-audit.md); [measured results](audit-load-summary.md) include raw JSON for all three optimized runs.

Compatibility: `cubicspline` and `cubic_spline` select the B-spline kernel.
The former `mitchell` alias is now rejected because Mitchell and B-spline
kernels differ; callers using that alias must choose a supported kernel.

The fixtures are small pixel-interleaved TIFFs, including one Deflate sample. They establish
deterministic correctness coverage, not comprehensive codec/planar-layout support.
The two optional large local-image regressions are not part of clean-checkout CI.
CI definitions have been added locally; no GitHub Actions run is claimed here.
