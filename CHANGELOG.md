# Changelog

All notable changes to this project are documented here.

## 0.2.0 - Unreleased

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
- `osmic_core::OsmicError`/`OsmicResult` were removed; every crate has its
  own typed error (`osmic-render` gained `RenderError`).
- `osmic-accel`: the error types are only exported at the crate root (the
  `error` module is private); a machine without a Metal device reports
  `NotAvailable` instead of `MetalInit`.
- `osmic-extract`: `Entity::osm_type` is an `OsmType` (serialized as
  before), `Entity::richness` returns a `Richness`, and the public option,
  result and entity structs are `#[non_exhaustive]` (start options from
  `Default`).
- `osmic::prelude` exports the tile renderer's settings as
  `TileRenderConfig`; the raster backend's `RenderConfig` is no longer in
  the prelude (`osmic::render::RenderConfig`).
- `osmic-app`: `Phase` is `#[non_exhaustive]`; calling `App::run` or
  `App::cleanup` from a plugin hook fails the build with
  `AppError::Reentrant`.
- An interrupted CLI command exits with 128 + the signal number (130 for
  Ctrl-C, 143 for SIGTERM); `osmic serve` drains and exits 0.
- `osmic-tiles`: `DecodedFeature::tags` holds typed `AttrValue`s (numbers
  and booleans are no longer strings).
- `osmic-style`: `PropertySource::property` returns a borrowed `ValueRef`;
  `EvalError` is an enum; `text-font` is an `Arc<[String]>`.
- `osmic-style`, `osmic-render`, `osmic-text`: public enums and config
  structs are `#[non_exhaustive]`; build `SceneOptions`, `RenderConfig`,
  `TessellationOptions` and `LabelStyle` with their constructors.
  `SkiaBackend::render_labels` takes a scale and offset instead of a
  tiny-skia `Transform`.
- `osmic-core`: `Zoom` is always within 0..=22. Its field is private:
  `Zoom::new` returns an `Option`, `Zoom::clamped` saturates, `Zoom::get`
  reads the level, and `TryFrom<u8>` replaces `From<u8>`.
- `osmic-tiles`: `RenderConfig`, `TileGeneratorConfig`, `TileSummary`,
  `ArchiveOptions` (build it with `ArchiveOptions::new`), `AttributeMode`,
  `TileCompression` and `TileFormat` are `#[non_exhaustive]`. The `sorter`
  and `proto` modules and `TileFeature::encode`/`decode` are private.
- `osmic-osm`: `Role::parse` returns a `Role`, with unknown roles as
  `Role::Other`; `PbfWriterOptions` and `UnknownLayer` are
  `#[non_exhaustive]`.
- `osmic-repl`: `UpdateOptions`, `ClientOptions` and `OscLimits` are
  `#[non_exhaustive]`; build them with their setters.
- `osmic-serve`: `ServeError::Archive` carries a typed `OpenArchiveError`.

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
- `osmic_tiles::reader` (feature `reader`) opens PMTiles archives after
  validating the header, with a bounded leaf-directory cache, and
  `decompress_tile` inflates tiles under a size cap. The server and the
  viewer read archives through it.
- `osmic serve` gained `--max-connections`, `--header-timeout`,
  `--drain-delay`, `--drain-timeout` and `--directory-cache`. Raster
  archives get raster tile URLs and a raster style.
- `PbfWriter` writes element metadata, LocationsOnWays and the header's
  `source`, and can copy encoded blobs verbatim.
- `Layer::from_name`, a lookup that never allocates; `load_geojson_with`
  takes a `TagRetention`.

### Changed

- Node locations are exact (1e-7°) instead of `f32`.
- Tag values are owned per feature and curated keys resolve without locks;
  each PBF block's string table is decoded once.
- Douglas–Peucker simplification is iterative on squared distances.
- The `osmic` binary uses mimalloc. PBF blocks inflate with zlib-rs.
- Dependencies updated to current releases, including wgpu 30, cosmic-text
  0.19, geo 0.33, rstar 0.13, pmtiles 0.24 and tower-http 0.7.
- `osmic update` copies the blocks no change touches verbatim, and
  re-encodes touched blocks with each object's metadata.
- Tile attributes are handled once per feature rather than once per tile
  piece (`Renderer::render` is about 3.5× faster on 200k buildings), and
  the sorter's leftover in-memory chunks are sorted in parallel.
- `osmic generate-tiles` keeps only curated tags for GeoJSON input unless
  `--all-tags` is given, as for PBF input.

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
- Rendering: numeric and boolean style filters now match MVT attributes;
  a NaN zoom no longer panics `interpolate`; exponential interpolation no
  longer yields NaN colors; huge halos, tiny dash patterns, deep
  expressions, huge label coordinates and far-off tile zooms no longer
  hang, overflow or exhaust memory; tile seams are not double-blended and
  clipping no longer allocates a full-size mask per tile; circle strokes,
  miter limits, background layers, `text-font`, `line-center`,
  `text-rotation-alignment`, `to-number`, `coalesce` and legacy `in`
  filters behave as in MapLibre.
- osmium's missing-location marker in LocationsOnWays files is no longer
  read as a coordinate, and out-of-range node coordinates are rejected
  instead of wrapping (tile and extract pipelines).
- Invalid UTF-8 elsewhere in a block no longer drops relation tags or
  roles. A multipolygon member with a misspelled role no longer drops the
  area, and self-intersecting rings reject the relation.
- `PbfWriter` caps blocks at 16 MiB, since readers refuse larger ones. An
  empty tag key no longer shifts the tags of the nodes that follow it.
- Change files must be well-formed osmChange documents within the OSM API
  limits, and a state file for the wrong sequence is rejected.
- Malformed PMTiles headers no longer panic the server or the viewer, and
  tile inflation is capped at 64 MiB.
- The server limits connections, header reads and shutdown draining.
  Brotli or zstd archives answer 406 instead of 500, and tile
  coordinates must be canonical.
- `TileGenerator::finish` no longer hangs when it is called on a rayon
  worker or when an encoder panics. Ring areas no longer overflow on
  extreme coordinates. `TileRange::len` and `is_empty` are correct for
  inverted ranges.

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
