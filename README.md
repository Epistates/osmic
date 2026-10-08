# osmic

[![Crates.io](https://img.shields.io/crates/v/osmic.svg)](https://crates.io/crates/osmic)
[![docs.rs](https://docs.rs/osmic/badge.svg)](https://docs.rs/osmic)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![MSRV 1.94](https://img.shields.io/badge/rust-1.94%2B-orange.svg)](https://www.rust-lang.org)

OpenStreetMap to vector tiles in Rust, plus the pieces around it: reading
PBF with exact coordinates, assembling multipolygons, classifying features,
generating PMTiles archives, serving them, keeping the source data current
from replication diffs, extracting entities, and rendering with MapLibre
styles.

**Status:** `0.2.x`, pre-1.0 — APIs may still change. See
[CHANGELOG.md](CHANGELOG.md).

## What it does

- **Tiles from any extract up to the planet's scale, in bounded memory.**
  Features are rendered while the PBF is read (each feature projected once,
  simplified per zoom, sliced into tiles by recursive band clipping) and
  spilled to a parallel external sort; tiles are then assembled, encoded and
  gzipped in parallel. Memory is set by `--memory-mb`, not by the input.
- **Exact data.** Node locations keep OSM's 1e-7° precision; a sparse node
  index needs about 9 bytes per node. Multipolygons and boundaries are
  assembled the way libosmium does it (role-agnostic rings, exact
  point-in-ring nesting, touching rings split, invalid and incomplete
  relations counted and reported).
- **Reproducible archives.** PMTiles output is Hilbert-ordered (clustered),
  byte-for-byte identical across runs and thread counts, written atomically,
  and its metadata lists the layers and fields actually present. Tiles over
  the size budget drop their least important features first.
- **Serving.** An HTTP server with strong ETags, gzip passthrough, CORS,
  timeouts, load shedding, graceful shutdown, health probes, TileJSON, a
  MapLibre style and a built-in viewer.
- **Staying current.** `osmic update` applies minutely/hourly/daily diffs
  from any replication server to a PBF file (pyosmium-up-to-date model:
  state lives in the PBF header, files are replaced atomically).
- **Styling and rendering.** A typed MapLibre style subset with an
  expression evaluator, a software renderer (tiny-skia), lyon tessellation,
  collision-aware labels and a wgpu viewer.

## Performance

United States extract (10.8 GB PBF, 1.51 billion nodes, 152 million ways)
to a z0–14 archive, on a 16-core Apple Silicon machine with 128 GB of RAM
that was otherwise heavily loaded during the run:

| | |
| --- | --- |
| Wall time | 5 min 43 s |
| CPU time | 22.6 min |
| Peak memory | 23 GB (13.1 GiB of it the node index) |
| Features | 163,084,065 |
| Tiles | 4,508,486 (11.5 GiB) |

## Command line

```sh
cargo install osmic-cli          # installs the `osmic` binary
```

```sh
# Vector tiles (all layers, z0-14) plus a MapLibre style for them
osmic generate-tiles region.osm.pbf region.pmtiles --style style.json

# Only some layers or zooms, or only features matching a tag filter
osmic generate-tiles region.osm.pbf roads.pmtiles --layers highway --zoom 6-14
osmic generate-tiles region.osm.pbf cafes.pmtiles --tags "amenity=cafe"

# Serve with a map viewer at http://127.0.0.1:3000/
osmic serve region.pmtiles

# Element counts, features per layer and data-quality report
osmic inspect region.osm.pbf

# Businesses and POIs to CSV, JSON or GeoJSON
osmic extract region.osm.pbf shops.csv --tags "shop=* office=*"

# Apply replication diffs (the PBF header carries the replication state)
osmic update region.osm.pbf
```

Every command has `--help`. Outputs are never overwritten without
`--force`; usage errors exit with code 2 and runtime errors with code 1;
`--log-format json` emits structured logs.

Layers: `highway`, `building`, `water`, `natural`, `landuse`, `railway`,
`amenity`, `leisure`, `boundary`, `place`, `shop`, `tourism`, `office`,
`healthcare`, `craft`, `historic`, `club`, `emergency`, `education`.

## Library

Use the umbrella crate, or depend on individual crates:

```toml
[dependencies]
osmic = "0.2"
```

Stream a PBF file into a PMTiles archive:

```rust,no_run
use std::path::Path;
use std::sync::Arc;

use osmic::osm::{PbfProcessor, PipelineConfig, TagRetention, TagStore};
use osmic::tiles::{ArchiveInfo, MvtEncoder, TileGenerator, TileGeneratorConfig};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let tags = Arc::new(TagStore::new());
    let tiles = TileGenerator::new(
        TileGeneratorConfig::default(),
        Box::new(MvtEncoder),
        Arc::clone(&tags),
    )?;
    let config = PipelineConfig::new().tag_retention(TagRetention::Curated);
    // Features go straight from the PBF reader into the tile renderer.
    PbfProcessor::with_tag_store(config, tags).run(Path::new("region.osm.pbf"), &tiles)?;
    let summary = tiles.write_pmtiles(Path::new("region.pmtiles"), &ArchiveInfo::default(), false)?;
    println!("{} tiles", summary.tiles);
    Ok(())
}
```

Or compose an application from plugins, here a tile server:

```rust,no_run
use osmic::prelude::*;

fn main() -> Result<(), AppError> {
    // Serve a PMTiles archive (with a MapLibre viewer at /) until Ctrl-C.
    App::new()
        .add_plugin(TileServerPlugin::new("tiles.pmtiles"))
        .run()
}
```

| Crate | Description |
| --- | --- |
| [`osmic`](crates/osmic) | Umbrella: re-exports and prelude |
| [`osmic-core`](crates/osmic-core) | Fixed-precision coordinates, typed OSM ids, geometry, Web Mercator, clipping, simplification |
| [`osmic-osm`](crates/osmic-osm) | PBF reading and writing, classification, multipolygon assembly, the two-pass feature pipeline |
| [`osmic-index`](crates/osmic-index) | Node location storage: sparse index and dense (memory or file) store |
| [`osmic-geo`](crates/osmic-geo) | Projection, simplification and ring orientation helpers |
| [`osmic-tiles`](crates/osmic-tiles) | Tile rendering, external sort, MVT/MLT encoding and decoding, PMTiles archives |
| [`osmic-serve`](crates/osmic-serve) | HTTP tile server |
| [`osmic-repl`](crates/osmic-repl) | Replication: change-file parsing and keeping PBF files current |
| [`osmic-extract`](crates/osmic-extract) | Entity extraction with tag filters, locations and deduplication |
| [`osmic-style`](crates/osmic-style) | Typed MapLibre style subset (parser, expression evaluator) and the default osmic style |
| [`osmic-render`](crates/osmic-render) | Style-driven scene building, software renderer, lyon tessellation, Mercator camera |
| [`osmic-text`](crates/osmic-text) | Shaping, collision-aware label placement, line labels |
| [`osmic-app`](crates/osmic-app) | Plugins, typed resources, events and a runner |
| [`osmic-accel`](crates/osmic-accel) | Opt-in Metal (Apple Silicon) geometry clipping with a CPU reference |
| [`osmic-cli`](bins/osmic-cli) | The `osmic` command |
| [`osmic-viewer`](bins/osmic-viewer) | Interactive wgpu map viewer for PMTiles archives |

The [`examples/`](examples/) directory has runnable programs:

```sh
cargo run --release -p load-pbf -- region.osm.pbf
cargo run --release -p render-static -- region.pmtiles map.png --bbox -122.52,37.70,-122.35,37.82
cargo run --release -p tile-server -- region.pmtiles --bind 127.0.0.1:3000
cargo run -p custom-plugin
cargo run --release -p osmic-viewer -- region.pmtiles
```

## Optional features

- `osmic-cli/mlt`, `osmic-tiles/mlt`: MapLibre Tile (MLT) output. Needs
  Rust 1.98, above the workspace MSRV.
- `osmic-osm/native` (enabled by the binaries): PBF reading. Without it,
  `osmic-osm` provides only the data model, without `osmpbf` or `rayon`.

## Building

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

The minimum supported Rust version is **1.94**. See
[CONTRIBUTING.md](CONTRIBUTING.md) for the full check list and
[SECURITY.md](SECURITY.md) for reporting vulnerabilities.

## License

MIT, see [LICENSE](LICENSE). Map data © OpenStreetMap contributors,
available under the [ODbL](https://www.openstreetmap.org/copyright).
