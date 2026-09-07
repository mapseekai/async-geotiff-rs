# Runtime limits and source consistency

These contracts apply to the audit changes in the working tree. They introduce
intentional compatibility changes: overload and malformed or mutable sources
now produce errors instead of unbounded work, partial tiles, or mixed versions.

## Active work and memory ownership

Default readers, tilers and mosaic pools share `WorkBudget::global()`, a
512 MiB allowance. `WorkBudget::new(bytes)` creates an independent allowance;
clone and inject the same budget through `ReaderConfig::work_budget`,
`Tiler::with_work_budget`, and `MosaicTiler::builder(...).work_budget(...)`
when these objects must share an application limit.

Reservations fail immediately with `AgError::ResourceLimit`; they do not queue
requests waiting for memory. `used_bytes()`, `peak_bytes()` and `limit_bytes()`
expose the accounting. Cloned reservations stay live until their last owner is
dropped, including an already-running blocking job after its caller cancels.
Managed response buffers retain their byte reservation for the buffer lifetime.

This is an active-work allowance, **not an OS RSS ceiling**. Completed
caller-owned images, resident decoded caches, allocator overhead, runtime/TLS
state and unrelated application allocations have different ownership. Set
`CacheConfig::max_bytes` for decoded-cache residency, share caches deliberately,
and bound how many completed outputs the application retains.

Reader defaults add per-operation validation:

| Setting | Default |
| --- | ---: |
| Source-window pixels | 16 × 1024 × 1024 |
| Window bytes / compressed fetch batch | 256 MiB |
| Blocks per window | 4096 |
| Declared decoded bytes per block | 64 MiB |
| Cumulative metadata bytes while opening | 8 MiB |
| IFDs | 256 |
| Entries per IFD | 4096 |
| Concurrent foreground windows | 8 |

Use `ReaderConfig::default()` and override fields explicitly. Oversized ranges,
metadata and arithmetic overflow fail before the corresponding fetch/allocation.
Background prefetch uses immediate budget acquisition and skips when foreground
work owns the allowance. These guards and a small workload are not a proof of
every codec/backend's behavior on arbitrary input.

The default decoder registration now bounds the output of uncompressed,
Deflate/OldDeflate, LZW, ZSTD and JPEG blocks against the declared block shape.
Typed/planar byte arithmetic and JPEG MCU padding are included in validation
and work estimates. This covers the registered implementations; it does not
make an arbitrary caller-supplied codec implementation safe.

## Request admission, deadlines and cancellation

The default `RequestLimiter` is shared across public tile calls and admits
64 requests. Excess calls return `AgError::Overloaded` immediately. Custom
limiters can be shared using `Tiler::with_request_limiter` and
`MosaicTilerBuilder::request_limiter`. Internal mosaic child work does not
consume a second request admission.

The default request timeout is 30 seconds, including I/O and permit waits.
`TilerConfig` and `MosaicConfig` expose `max_in_flight_requests` and
`request_timeout`; use `..Default::default()` when constructing these configs.
`tile_with_options` (and single-source `warp_tile_with_options`) accepts a
`RequestOptions` containing a per-call timeout and a cloneable
`RequestCancellation`. Calling `cancel()` on any clone ends the request with
`AgError::Cancelled`; expiration returns `AgError::DeadlineExceeded`.

Cancellation is cooperative. It stops later pipeline stages and drops pending
I/O, but does not preempt an arbitrary running CPU instruction or remote server
handler. Blocking work retains its admission/accounting until it finishes.
HTTP client/TLS initialization runs off async workers under a separate bounded
limit; local workload telemetry distinguishes request return from later activity.

The example server exposes:

- `ASYNC_GEOTIFF_MAX_IN_FLIGHT_REQUESTS` (positive integer).
- `ASYNC_GEOTIFF_REQUEST_TIMEOUT_MS` (positive integer milliseconds).

HTTP admission and timeout include dataset lookup/opening. Library callers that
open a `CogReader` directly outside a tile request should apply their own outer
timeout when an application deadline must include that initial open.

## Pinned object snapshots

Managed `open_http` and `open_object_store` use
`SnapshotPolicy::RequireValidator` by default. A strong ETag or immutable object
version must be available. Conditional reads and decoded-cache identity pin
metadata and pixels to that snapshot; a changed source produces an error
instead of a mixed-version tile. Version-aware stores may continue serving the
pinned old version. LocalFileSystem managed opens supply validators, so the
checked-in fixtures do not need to waive this contract.

An opaque `ObjectReader` does not expose its underlying store for conditional
requests. Passing it to `.open(reader)` therefore requires explicit
`.snapshot_policy(SnapshotPolicy::AssumeImmutable)`. This is a caller guarantee,
not verified snapshot consistency. Use it only for a source the caller keeps
immutable, or migrate to a managed store open.

Validator consistency depends on truthful validators and the backend's atomic
write contract. It cannot detect a server that changes bytes while reusing the
same validator, or raw in-place filesystem writes that violate that contract.

There is no automatic source refresh. Reopen a direct reader to discover the
current validator. For mosaics, call `invalidate_source(key, source_id)` so the
next lookup opens a fresh snapshot, or use a new source version identity.
Invalidation does not mutate an existing reader `Arc`: it retains its original
snapshot, and uncached reads fail if the old version is no longer available.
New reader/cache identities include the validator and size, preventing stale
decoded blocks from being reused as new imagery.

## Verification scope

The fixtures cover known UInt8/UInt16 pixels, overviews, nodata, projected data,
and one small Deflate stream. See [the workload procedure](load-audit.md) for
latency/RSS measurement scope and [the verification ledger](audit-verification.md)
for final test results. Synthetic local measurements do not establish production
capacity or complete malformed-codec coverage.
