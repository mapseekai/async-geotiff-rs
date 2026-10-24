---
title: Releases and crates.io
description: Package contents, verification and publication of async-geotiff.
weight: 40
---

The package name is `async-geotiff`; Rust imports use `async_geotiff`.
Version 0.13.0 prepares the first crates.io distribution. Older GitHub releases
remain available as source archives. Making the GitHub repository public does
not itself publish a crate; Cargo uploads a separate `.crate` archive.

## Package contents {#contents}

`Cargo.toml` uses an explicit `include` list: library sources, examples and
their HTML viewers, tests and small reproducible fixtures, the resampler
benchmark, manifests, READMEs, changelog and the MIT license text.
Tests and fixtures stay together because unit tests and examples embed fixture
bytes. `website/`, scripts, agent instructions, local imagery and generated
reports are not in the registry package.

`benches/resampler_bench.rs` measures six useful kernel cases and is retained.
`scripts/run_load_audit.py` records bounded local latency and peak RSS; it is
retained in Git, with output under ignored `target/load-audit/`. Historical
reviews and machine-specific results are available in Git history through
`7170204`, rather than in the current source tree. See the
[workload guide](/docs/guides/local-workload/) for reproducible measurements.

## Verify a version {#verify}

Update `Cargo.toml`, `Cargo.lock`, the changelog and both website languages.
Run the five feature configurations documented in `AGENTS.md`, including
all-target and documentation tests, plus the viewer and strict Hugo builds.
Then inspect and compile the actual registry archive:

```sh
cargo +stable package --list --locked
cargo +stable publish --dry-run --locked
```

The `.crate` file is generated under `target/package/`. For local review before
committing, add `--allow-dirty`; omit it for the final publication. The
docs.rs build disables default features to avoid requiring native PROJ while
documenting mosaics, STAC and image encoders. Applications still get the
normal default features.

## Publish {#publish}

After verification, commit the release and push the matching version tag:

```sh
git tag v0.13.0
git push origin main v0.13.0
```

The `Release` workflow validates the tag against `Cargo.toml`, installs native
build dependencies, runs tests, verifies the archive and attaches it to a
GitHub Release. A successful release workflow deploys the exact commit's
documentation to Cloudflare Pages. Pushing a tag does not upload to crates.io.

For the registry upload, use a crates.io account with a verified email and a
token allowed to publish this crate:

```sh
cargo login
cargo +stable publish --locked
```

Alternatively, configure the repository secret `CARGO_REGISTRY_TOKEN`, open
the `Release` workflow in GitHub Actions, select the matching **version tag**,
and enable `publish_to_crates_io`. The workflow repeats verification before
uploading. Do not paste tokens into commands, logs or committed files.

Published versions cannot be overwritten; verify the contents and version
before uploading. See the [Cargo publishing guide](https://doc.rust-lang.org/cargo/reference/publishing.html).

## Upgrade from 0.12 {#migration}

Raster Style remains SDK 0.4.0 / protocol 3.0/Q3. For custom STAC clients,
replace the built-client `with_http_client` constructor with
`with_http_client_builder`, passing a configured reqwest 0.11 `ClientBuilder`.
The old constructor returns an error. See [mosaics](/docs/guides/mosaics/) for
the authentication and redirect contract. The release also fixes independent
mask grids, polar coverage, Alpha point queries, exact-grid CubicSpline,
extreme finite resampling, histogram counts and server configuration propagation.
