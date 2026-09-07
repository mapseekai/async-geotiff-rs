# Local load results

Measured on 2026-09-07 on macOS-15.6.1-arm64-arm-64bit-Mach-O, 10 logical CPUs, optimized release binary with default features plus mosaic. Each scenario has 32 samples; the worktree was dirty at commit `82f9bca9b8a059515be65df4c117916a1caa6942`. No concurrent builds from our agents ran during measurement.

Executable SHA256: `2277b97ada46f7ecf28879b87306fdd6639fc27d95184f749b44b0dd18e5f6c1`. Exact environment and commands are in [C1](audit-load-c1.json), [C4](audit-load-c4.json), and [C8](audit-load-c8.json).

The driver calls library tile APIs against a local synthetic TIFF server with an injected 2 ms GET delay. It is not an example-server HTTP throughput test. See [the procedure and limits](load-audit.md).

| Concurrency | Whole-run peak RSS (MiB) | Process wall time (s) |
| --- | ---: | ---: |
| 1 | 27.03 | 11.86 |
| 4 | 30.72 | 7.71 |
| 8 | 31.67 | 7.46 |

| Concurrency | Scenario | P50 ms | P95 ms | P99 ms | GETs | New decoded blocks |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| 1 | single_cold | 101.947 | 156.545 | 194.350 | 96 | 128 |
| 1 | single_warm | 0.199 | 0.257 | 0.612 | 0 | 0 |
| 1 | mosaic_cold | 171.611 | 179.435 | 256.918 | 288 | 384 |
| 1 | mosaic_warm | 0.311 | 0.368 | 0.573 | 0 | 0 |
| 4 | single_cold | 155.298 | 212.430 | 213.624 | 96 | 128 |
| 4 | single_warm | 0.259 | 0.323 | 0.343 | 0 | 0 |
| 4 | mosaic_cold | 427.265 | 494.599 | 501.975 | 288 | 384 |
| 4 | mosaic_warm | 0.858 | 1.111 | 1.113 | 0 | 0 |
| 8 | single_cold | 274.273 | 377.998 | 399.879 | 96 | 128 |
| 8 | single_warm | 0.599 | 0.763 | 0.877 | 0 | 0 |
| 8 | mosaic_cold | 703.273 | 964.796 | 1011.665 | 288 | 384 |
| 8 | mosaic_warm | 1.396 | 1.568 | 1.597 | 0 | 0 |

Each controlled cancellation/deadline case launched eight requests against two admission permits: six were rejected and two terminated. Across all three runs:

- A 10 ms deadline returned after 11.86–12.25 ms.
- A cancellation requested after 20 ms returned after 21.18–22.38 ms from request start.
- Both permits were available when callers returned and after 250 ms.
- No new GETs or decodes were observed after return during that settling window. Already-started remote handlers are a separate lifecycle.
- Eight 1024-pixel tile requests against a 64 KiB budget were rejected before pixel GETs in every run; retained budget bytes were zero afterward.

The RSS value covers the whole child process run, not an individual scenario. Work-budget accounting is distinct from RSS and completed-output/cache ownership. With 32 samples, nearest-rank P99 is the maximum; these data are not a stable production tail or capacity estimate. Cold samples include reader/client/metadata setup; warm samples reuse readers and decoded blocks. The tests do not attempt to preempt a running codec CPU instruction or generate an unbounded request stream.
