# Repository Guidelines

## Project Structure & Module Organization

`src/` holds COG reading/caching (`io/`), reprojection (`warp/`), rendering (`render/`), Raster Style validation (`style/`), and mosaics (`mosaic/`). Keep HTTP serving in `examples/serve.rs` and `examples/serve/server.rs`; viewers live in `examples/*.html`. Tests/fixtures belong in `tests/`, benchmarks in `benches/`, technical reports in `docs/`, load-audit tooling in `scripts/`, and bilingual Hugo documentation in `website/`.

## Build, Test, and Development Commands

Use Rust 1.92+. Default features enable `proj` and `mosaic`; native dependencies are documented in `README.md`.

- `cargo build --locked`: build with default features; add `--no-default-features` to omit PROJ and mosaic support.
- `cargo +stable fmt --all -- --check`: verify Rust formatting.
- `cargo +stable check --locked --all-targets`: check library, examples, tests, and benchmarks.
- `cargo +stable test --locked --all-targets`: run unit and integration tests and benchmark checks.
- `cargo +stable test --locked --doc`: run documentation tests.
- `node --test examples/*.test.cjs`: check viewer startup ordering.
- `cargo run --release --example serve`: serve local `data/*.tif` at `127.0.0.1:8080`; optionally append `-- <COG_URL>`.
- `cargo bench --bench resampler_bench`: run Criterion benchmarks.

## Coding Style & Naming Conventions

Follow Rust 2021 conventions and rustfmt’s four-space indentation. Use `snake_case` for modules/functions, `PascalCase` for types, and `SCREAMING_SNAKE_CASE` for constants. Reuse `AgError`/`AgResult`, preserve feature gates, and keep CPU-heavy work behind existing blocking/concurrency limits.

## Testing Guidelines

Use `#[test]` or `#[tokio::test]`, with behavior-based names and regression coverage. Regenerate checked-in fixtures with `python3 tests/fixtures/generate.py`. Explicitly ignore tests requiring large local imagery, documenting the required filename.

Name tests after their source, never a scenario or review. Private Rust tests stay inline or in adjacent `<source>_test.rs` (`mod_test.rs` for `mod.rs`). Public tests mirror `src/` under `tests/`, with explicit Cargo targets. Each source has at most one standalone test file; inline private tests when a public suite already exists. HTML tests use adjacent `<source>.test.cjs`; shared helpers belong in `tests/support/`. Preserve test cases, feature gates, and historical validation manifests when moving files.

Mirror CI’s feature matrix: default, `--no-default-features`, `--no-default-features --features mosaic`, `--features stac,webp,jpeg`, and `--all-features`. Run both target and documentation tests for affected configurations.

## Commit & Pull Request Guidelines

Use `feat:`, `fix:`, `docs:`, or `chore:`, with optional scopes such as `fix(style):`. PRs describe behavior changes, link issues, and list validation commands/features. Include viewer screenshots and update English/Chinese documentation for public changes.

## Repository Instructions & Issue Tracking

Keep root `AGENTS.md` authoritative and `CLAUDE.md` a relative symlink to it.
