# Changelog

All notable changes to this project are documented here.

## Unreleased

This release rewrites most of the pipeline for correctness and scale. On the
United States extract (10.8 GB PBF) it generates z0–14 tiles in under six
minutes with 23 GB peak memory; the previous release could not finish (its
dense node store alone reserved 97 GB).

### Breaking

- The CLI installs as `osmic` (was `osmic-cli`) and refuses to overwrite
  outputs without `--force`. Usage errors exit with 2, runtime errors with 1.
- `osmic-app`: `Plugin::build`/`finish` return `Result`; `ready()` was
  removed; `App::run`/`set_runner` added. The empty `HeadlessPlugins` and
  `DefaultPlugins` groups were removed.
- `osmic-osm`: features carry typed `OsmId`s and one feature per matching
  layer; `Tags` own their values (`TagValue` is `SmolStr`) and only keys are
  interned in `TagStore`.
- `osmic-tiles`: new streaming `TileGenerator`; the external sorter returns
  `SortedRuns` read back by partition.
- `osmic-repl` was rewritten around `update_pbf`; the redb feature store and
  dirty-tile tracking were removed.
- `osmic-style`, `osmic-render`, `osmic-text`: typed style model with an
  expression evaluator; scene building, tessellation and label placement
  APIs replace the previous placeholders.
- The `mlt` feature needs Rust 1.98 (`mlt-core` 0.19).

### Added

- Sparse node index (~9 B/node, no id cap) alongside dense memory and file
  stores; `--node-store` selects one.
- libosmium-style multipolygon and boundary assembly with counted
  incomplete/invalid relations; history files are rejected.
- Tile rendering while the PBF is read, a parallel external sort with a
  memory budget (`--memory-mb`), size-budgeted tiles that drop their least
  important features, clustered and reproducible PMTiles with accurate
  metadata, and a hardened MVT decoder.
- `osmic serve`: ETags, gzip passthrough, CORS, timeouts, load shedding,
  graceful shutdown, `/healthz`/`/readyz`, TileJSON 3.0.0 and a viewer.
- `osmic update`: replication diffs applied to a PBF file with the state in
  its header (https-only by default, size limits, retries).
- `osmic extract`: CSV/JSON/GeoJSON with formula-injection protection, way
  and relation locations and Unicode-normalised deduplication.
- `TileServerPlugin` serves as the app runner; `ServerRoutes` lets other
  plugins mount routes behind the server's middleware.
- MapLibre style subset (filters and expressions), software renderer with
  dashes, halos and clipping, line-following labels, and a Mercator wgpu
  viewer with background tile loading.
- CI on Linux, macOS and Windows with MSRV, docs, cargo-deny and coverage;
  workspace lints; CONTRIBUTING and SECURITY policies.

### Changed

- Node locations are exact (1e-7°) instead of `f32`.
- Tag values are owned per feature and curated keys resolve without locks;
  each PBF block's string table is decoded once.
- Douglas–Peucker simplification is iterative on squared distances.
- The `osmic` binary uses mimalloc. PBF blocks inflate with zlib-rs.
- Dependencies updated to current releases, including wgpu 30, cosmic-text
  0.19, geo 0.33, rstar 0.13, pmtiles 0.24 and tower-http 0.7.

### Fixed

- Ways crossing tile edges keep every piece; features near an edge reach the
  neighbouring tile's buffer; MVT winding is enforced.
- Invalid UTF-8 in a PBF string table no longer drops an element's remaining
  tags.
- Outputs are written atomically, also on filesystems without no-replace
  renames (exFAT, FAT).
- Plugins added from another plugin's `build` were silently dropped.
- `scripts/publish.sh` published crates in an order that no longer matched
  their dependencies.

## 0.1.1 - 2026-05-07

### Added

- Added README badges for crates.io, docs.rs, license, and MSRV.
- Added a runnable `custom-plugin` example that registers a plugin and resource.
- Added regression coverage for multiple external tile sorters sharing a temp directory.

### Changed

- Bumped workspace crates, internal path dependency versions, and examples to `0.1.1`.
- Moved `NodeLocationStore` into `osmic-core` and re-exported it from the native OSM pipeline.
- Made `osmic-osm/native` opt-in at the workspace dependency level.
- Updated README quickstart and examples to match the current app lifecycle and PMTiles renderer.

### Fixed

- Fixed external sort chunk-file collisions when independent sorters use the same temp directory.
- Fixed `osmic-osm` and `osmic-tiles` `--no-default-features` builds.
- Fixed Clippy warnings across OSM assembly, extraction, and viewer input handling.
