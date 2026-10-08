# Contributing

Thanks for helping improve osmic. This document covers how to build, test
and submit changes.

## Building and testing

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

Optional features are tested with `--all-features`. The `mlt` feature
(MapLibre Tile output) depends on `mlt-core`, which needs Rust 1.98; every
other feature builds with the MSRV.

`cargo deny check` verifies licenses and advisories (configured in
`deny.toml`). CI runs all of the above on Linux, macOS and Windows, plus an
MSRV check, `cargo doc` with warnings denied and coverage.

## Minimum supported Rust version

The MSRV is **1.94** (see `rust-version` in `Cargo.toml`). Raising it is a
deliberate change: note it in `CHANGELOG.md` and the README badge.

## Performance

Correctness, efficiency and performance are the project's priorities.
Changes to the OSM or tile pipelines should be measured on a real extract
(a state or country from [Geofabrik](https://download.geofabrik.de/) is a
good size) with a release build:

```sh
cargo build --release -p osmic-cli
/usr/bin/time -l target/release/osmic generate-tiles region.osm.pbf out.pmtiles
```

Report wall time, peak memory and the per-phase timings the CLI logs at
`info` level, before and after.

## Code style

- Match the conventions of the surrounding code; `cargo fmt` is the source
  of truth for formatting.
- Library crates return typed errors (`thiserror`); only binaries use
  `anyhow`. Library code does not panic on bad input.
- Public items have doc comments. Prefer a test that reproduces a bug
  before fixing it.
- Writers create outputs atomically through `osmic_core::fs::temp_file_for`
  and `osmic_core::fs::persist`.

## Commits and pull requests

Commit messages follow [Conventional Commits](https://www.conventionalcommits.org/)
(`feat(tiles): …`, `fix(osm): …`, `chore(deps): …`). Keep each pull request
focused on one change, and add an entry under `Unreleased` in
`CHANGELOG.md` for anything user-visible.

## Releasing

Maintainers publish with `scripts/publish.sh`, which derives the publish
order from the workspace manifests and resumes safely after a failure.
