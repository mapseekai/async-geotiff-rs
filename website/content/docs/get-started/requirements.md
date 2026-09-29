---
title: Requirements
description: Rust, system libraries, and optional documentation tooling.
weight: 10
---

## Library and examples {#library}

- Rust 1.92 or newer, matching `Cargo.toml`.
- The default feature set requires system `libproj` 9.x.
- Git is required when working from the repository.

On macOS:

```bash
brew install proj
```

On Debian or Ubuntu:

```bash
sudo apt install libproj-dev libsqlite3-dev libtiff-dev clang
```

On other platforms, install `libproj` with the platform's supported package
manager or build without the default features. The project does not currently
provide platform-specific Windows installation commands.

If you only need same-CRS tiles or the built-in EPSG:4326 ↔ EPSG:3857 fast
path, build without the default features:

```bash
cargo build --no-default-features
```

## Documentation site {#documentation}

The OINK site under `website/` requires Go 1.27 or newer and Hugo Extended
0.165.0 or newer. Hugo must report `extended` in its version string.

```bash
go version
hugo version
```

The library itself does not depend on Hugo or Go.
