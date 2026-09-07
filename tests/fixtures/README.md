# Synthetic correctness fixtures

These TIFFs are generated entirely from known values; they contain no external imagery.
Regenerate with `python3 tests/fixtures/generate.py` (standard library only).
The files are checked in, so `cargo test` requires no fixture setup or downloads.

Each has a 64 × 64 base image split into four 32 × 32 tiles and one 32 × 32
overview. Samples use an integer gradient with zero nodata in the southwest
corner. All IFDs are little endian and pixel interleaved.

- `rgb_nodata.tif`: three UInt8 bands, EPSG:4326, bounds [10, 20, 10.064, 20.064].
- `uint16.tif`: one UInt16 band, the same geographic bounds.
- `projected.tif`: three UInt8 bands, EPSG:3857, 100 metre base pixels near 10°E, 20°N.
- `rgb_deflate.tif`: the RGB geographic sample with TIFF Deflate compression.
  Other samples are uncompressed. Deflate generation uses stdlib zlib level 9;
  byte-level reproducibility also depends on the zlib implementation/version.

Missing required files must fail tests. Large local imagery regressions belong in
explicitly ignored tests with the required filename documented in the ignore reason.
