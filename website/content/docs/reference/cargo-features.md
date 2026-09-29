---
title: Cargo features
description: Compile-time capabilities and their dependency effects.
weight: 20
---

| Feature | Default | Effect |
| --- | ---: | --- |
| `proj` | yes | Enables general CRS transformation through system `libproj`. |
| `mosaic` | yes | Enables `MosaicTiler`, `MosaicSource`, MosaicJSON, and async asset fan-out. |
| `webp` | no | Enables lossless WebP encoding through `image`. |
| `jpeg` | no | Enables the low-level JPEG encoder; Raster Style requests currently reject JPEG. |
| `stac` | no | Adds the STAC client and implies `mosaic`. |
| `perf-tracing` | no | Emits detailed internal timing events. |
| `tokio-console` | no | Enables Tokio Console integration and implies `perf-tracing`. |

Examples:

```bash
# Default: projection + mosaics + PNG
cargo build --release

# Minimal library without general PROJ or mosaics
cargo build --release --no-default-features

# Example server with all image formats
cargo run --release --example serve --features webp

# STAC-backed mosaic demo
cargo run --release --example serve --features stac

# Full verification surface
cargo test --all-features --locked
```

The `serve`, `mosaic_source`, and `mosaic_json` examples require the `mosaic`
feature. PNG support is always compiled.
