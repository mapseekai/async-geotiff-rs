# Reproducible local workload

Build the optimized binary separately from measurement:

```sh
cargo build --release --locked --example load_audit --features mosaic
python3 scripts/run_load_audit.py --samples 32 --concurrency 4 --output docs/audit-load-c4.json
```

Repeat with concurrency 1 or 8 and a different output filename. The wrapper
rejects more than 128 samples or concurrency above 16 and kills a run after
120 seconds. All network traffic goes to an ephemeral loopback server in the
workload process. No credentials or public datasets are required.
The driver calls the library tile APIs directly; the HTTP server supplies TIFF
bytes. This is not an end-to-end load test of the example server's tile routes.

The 64×64 synthetic Deflate TIFF has a 32×32 overview, three bands, four base
blocks and a nodata corner. The server adds 2 ms to each GET. Cold samples
include reader/client creation and metadata; warm samples use an already
rendered tile's reader and decoded blocks. Mosaic samples use three separately
identified URLs with identical fixture pixels and the Mean reducer. Outputs
are 64×64 PNGs. Samples run in bounded batches; throughput includes batch
barriers and is not an open-loop arrival-rate test.

Each latency table reports nearest-rank P50/P95/P99. With 32 samples, P99 is
the maximum sample; do not interpret it as a stable production tail estimate.
The JSON includes cache counters before/after, individual cold cache snapshots,
GET counts, prepared response bytes, maximum active server handlers, and the
global work-budget estimate (its peak is cumulative across scenarios). “Response bytes” means payloads prepared by the
server, not bytes proven delivered to a cancelled client.

Cancellation cases use a 100 ms server delay. One case aborts task futures;
others use an explicit cancellation handle (20 ms) or a deadline (10 ms).
The latter launch eight requests against an admission limit of two and record
all rejection/termination outcomes. Single-source readers are opened first so
these cases exercise in-flight tile reads. Cold mosaic cases additionally
cover client/metadata setup. Counters are sampled at return and after a 250 ms
settling period; admitted-request permits and work-budget bytes are recorded.
Remote server handlers may finish after client cancellation. These counters
do not measure every background TLS initialization job or prove OS thread
preemption of a running decoder.

A memory-admission case submits eight 1024×1024 renders to a 64 KiB work budget.
It requires eight resource-limit errors, no pixel GETs, and zero retained work
bytes afterward. This checks early rejection, not peak memory under a full budget.

The wrapper records UTC time, OS/architecture, logical CPU count, Rust version,
commit and dirty status, executable SHA256, fixture SHA256, build-profile label,
and child user/system CPU time. Peak RSS is the workload process's `wait4`
`ru_maxrss` (bytes on macOS; KiB converted on Linux); compiler memory is excluded.
It is a whole-run peak, not a per-scenario value or a process-tree total.

These are bounded synthetic measurements on one machine. They do not establish
production capacity, an adversarial codec memory bound, or behavior under an
unbounded request stream. Fixture regeneration uses Python's standard library;
Deflate bytes also depend on the installed zlib implementation.
