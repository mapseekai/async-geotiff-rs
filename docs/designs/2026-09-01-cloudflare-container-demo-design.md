# Cloudflare Container Demo Design

## Status

Approved design for implementation planning. The target is a publicly reachable
interactive deployment of the existing Axum example; the already deployed
Cloudflare Pages project remains the static documentation site.

## Goal

Expose the real `examples/serve.rs` application at a Cloudflare `workers.dev`
URL so readers can use `/maplibre`, `/openlayers`, `/leaflet`, and the tile API
without running the repository locally.

## Non-goals

- Replace the Rust/PROJ render pipeline with Workers or WebAssembly.
- Let public requests choose arbitrary COG, STAC, or MosaicJSON URLs.
- Add custom domains, authenticated access, persistent raster storage, or
  horizontal autoscaling in the first version.

## Chosen approach

Deploy the existing Axum example in one Cloudflare Container behind a thin
Worker router. This preserves the current COG, cache, tile, viewer, and CORS
behavior while satisfying the `libproj` system dependency in a normal Linux
container.

The alternative approaches were rejected:

1. Rewriting for Workers/WASM would require replacing native TIFF, Tokio, and
   PROJ execution paths.
2. A non-Cloudflare container host would not meet the selected platform target.

## Public contract

- A Worker named `async-geotiff-demo` receives the default `workers.dev` URL
  assigned by Cloudflare at deployment time.
- The documentation site links to that URL after the first successful deploy.
- The Worker forwards `/`, `/maplibre`, `/openlayers`, `/leaflet`,
  `/tiles/...`, `/mosaic/tiles/...`, and support routes to the container.
- The container runs one fixed public COG:

  ```text
  https://raw.githubusercontent.com/cogeotiff/rio-tiler/0b08b7f35a8b639cee2f35a0cb565034f7b55bfd/tests/fixtures/cog.tif
  ```

  This small, range-capable fixture is suitable for a public functional demo;
  it is not a general-purpose data service.

## Runtime design

```text
Browser
  -> async-geotiff-demo.<account>.workers.dev
  -> Worker fetch handler
  -> Durable Object container binding: getByName("demo")
  -> startAndWaitForPorts([8080])
  -> Axum examples/serve binary
  -> fixed remote COG via HTTPS range requests
```

The container class configures:

- `defaultPort` and `requiredPorts` as `8080`;
- `enableInternet: true` for COG range reads;
- `pingEndpoint: "/formats"`, which does not depend on user input;
- `sleepAfter: "10m"`;
- one `basic` instance (1 GiB memory) with `max_instances: 1`.

The singleton routing key intentionally shares the process-wide decoded-block
cache. The first request after inactivity can incur a container cold start.

The Worker catches startup/proxy failures and returns a concise `503 Demo
temporarily unavailable` response rather than exposing container internals.
The Axum example continues to own its existing request parsing and error
responses.

## Container image

`Dockerfile.demo` lives at the repository root so its build context includes
the current Rust workspace. It uses a multi-stage build:

1. Build stage installs the Rust toolchain and native PROJ build dependencies,
   then runs `cargo build --release --example serve`.
2. Runtime stage contains only the compiled example and the runtime PROJ
   libraries supplied by the selected Debian-family base image.
3. The entrypoint sets `ASYNC_GEOTIFF_BIND=0.0.0.0:8080` and invokes the
   release `serve` binary with `DEMO_COG_URL`.

The demo URL is baked into the Worker container environment, not accepted from
HTTP requests. No credentials are needed for the selected COG.

## Repository structure

```text
Dockerfile.demo                  Rust example image
wrangler.demo.jsonc              Worker, Container, DO binding, migration
cloudflare-demo/
  package.json                   @cloudflare/containers and Wrangler tooling
  src/index.ts                   singleton proxy Worker and 503 handling
  start-demo.sh                  container entrypoint
.github/workflows/cloudflare-demo.yml
                                 deploy on relevant main changes
```

`wrangler.demo.jsonc` stays at the repository root so the Docker image path
uses the repository root as its build context. The Worker source resolves its
dependencies from `cloudflare-demo/node_modules`.

## Deployment automation

`.github/workflows/cloudflare-demo.yml` runs on `main` only when one of these
areas changes:

- `src/**`, `examples/**`, `Cargo.toml`, or `Cargo.lock`;
- `Dockerfile.demo`, `wrangler.demo.jsonc`, or `cloudflare-demo/**`;
- the demo deployment workflow itself.

The workflow installs the pinned Node dependencies and deploys with Wrangler.
It requires the existing repository secret `CLOUDFLARE_API_TOKEN`; the token
must have account-level Cloudflare Containers, Workers, and Durable Objects
write access sufficient for this deployment. The account ID is non-secret
configuration. Documentation-only changes do not rebuild the container.

## Verification

Before deployment:

1. Build the container image locally or in CI.
2. Compile/check the Worker and validate Wrangler configuration.
3. Verify the existing Rust test suite to the extent supported by the host;
   the known macOS headless STAC constructor failure remains tracked as
   `MAP-170` and is unrelated to this demo.

After deployment:

1. Request `/formats` and expect a successful response.
2. Load `/maplibre` and verify it returns HTML.
3. Fetch one `WebMercatorQuad` PNG tile and verify an image response.
4. Add the resolved `workers.dev` URL to the OINK documentation and verify the
   external link from the public Pages site.

## Risks and operating constraints

- Cloudflare Containers is beta and has no SLA; deployment APIs may change.
- The container disk is ephemeral. The current demo does not rely on it.
- A single instance trades peak throughput for bounded resource use and a
  shared cache. It is intentionally a demo, not a production tile service.
- The configured COG remains a third-party public fixture; if it disappears,
  the demo should return an operational error rather than retry arbitrary URLs.
