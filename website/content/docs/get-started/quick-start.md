---
title: Quick start
description: Run the example server and request your first rendered tile.
weight: 20
---

## Clone and run {#run}

```bash
git clone https://github.com/mapseekai/async-geotiff-rs.git
cd async-geotiff-rs
COG_URL='https://raw.githubusercontent.com/cogeotiff/rio-tiler/0b08b7f35a8b639cee2f35a0cb565034f7b55bfd/tests/fixtures/cog.tif'
cargo run --release --example serve -- "$COG_URL"
```

The pinned 800 KiB `rio-tiler` fixture is a public, range-capable COG intended
for tests and first-run verification. Replace it with your own COG after the
smoke test. The server listens on `127.0.0.1:8080` by default and prints the
available viewer and tile URLs when startup completes.

Open one of the included viewers:

- <http://127.0.0.1:8080/maplibre>
- <http://127.0.0.1:8080/openlayers>
- <http://127.0.0.1:8080/leaflet>

The viewer automatically centers on the opened dataset and is the most reliable
visual smoke test. You can also request the world tile directly:

```bash
curl --fail --output tile.png \
  'http://127.0.0.1:8080/tiles/WebMercatorQuad/0/0/0.png'
```

Success produces a PNG file that you can open in any image viewer. A valid tile
coordinate outside the source extent returns HTTP 204 instead of a synthetic
blank image; if that happens, use one of the included viewers to request an
in-bounds tile at the dataset's calculated center and zoom.

## Browse local files {#local-files}

Run the server without a URL to discover `data/*.tif` and `data/*.tiff` files:

```bash
mkdir -p data
cp /path/to/example.tif data/
cargo run --release --example serve
```

The viewers then display a dataset switcher.

## Optional formats {#formats}

PNG is always available. Enable lossless WebP explicitly:

```bash
cargo run --release --features webp --example serve -- \
  https://example.com/cog.tif
```

The `/formats` endpoint reports only the encoders compiled into the running
binary.
