# osmic

[![Crates.io](https://img.shields.io/crates/v/osmic.svg)](https://crates.io/crates/osmic)
[![docs.rs](https://docs.rs/osmic/badge.svg)](https://docs.rs/osmic)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![MSRV 1.94](https://img.shields.io/badge/rust-1.94%2B-orange.svg)](https://www.rust-lang.org)

A Rust workspace for working with OpenStreetMap data end-to-end: parsing PBF,
building spatial indexes, generating vector tiles, styling, rendering, and
serving.

**Status:** early — `0.1.1`, APIs will change.

See [CHANGELOG.md](CHANGELOG.md) for release notes.

## Installation

Use the umbrella crate for the full SDK:

```toml
[dependencies]
osmic = "0.1.1"
```

Or pull in individual crates as needed:

```toml
[dependencies]
osmic-core = "0.1.1"
osmic-osm = "0.1.1"
osmic-tiles = "0.1.1"
```

## Workspace layout

| Crate | Description |
| --- | --- |
| [`osmic`](crates/osmic) | Umbrella: re-exports and prelude |
| [`osmic-core`](crates/osmic-core) | Shared types, errors, coordinate primitives |
| [`osmic-osm`](crates/osmic-osm) | OSM data model, PBF parsing, tag system, classification |
| [`osmic-geo`](crates/osmic-geo) | Projection, simplification, clipping, validation |
| [`osmic-index`](crates/osmic-index) | Node location store (mmap) and spatial R-tree index |
| [`osmic-style`](crates/osmic-style) | MapLibre-compatible style system |
| [`osmic-tiles`](crates/osmic-tiles) | MVT/MLT encoding, PMTiles archives, tile math |
| [`osmic-render`](crates/osmic-render) | Backend abstraction, scene graph, tessellation |
| [`osmic-text`](crates/osmic-text) | Text shaping, label collision, glyph atlas |
| [`osmic-app`](crates/osmic-app) | Plugin system, App builder, event bus, lifecycle |
| [`osmic-serve`](crates/osmic-serve) | HTTP tile server with Tower middleware pipeline |
| [`osmic-extract`](crates/osmic-extract) | Business entity extraction from OSM PBF |
| [`osmic-accel`](crates/osmic-accel) | GPU-accelerated geometry processing (Apple Silicon Metal) |
| [`osmic-repl`](crates/osmic-repl) | OSM replication and incremental tile updates |
| [`osmic-cli`](bins/osmic-cli) | CLI for tile generation, PBF inspection, render-to-image |
| [`osmic-viewer`](bins/osmic-viewer) | Interactive wgpu windowed map viewer |

## Quickstart

```rust,no_run
use osmic::prelude::*;

fn main() -> Result<(), AppError> {
    // Serve a PMTiles archive (with a MapLibre viewer at /) until Ctrl-C.
    App::new()
        .add_plugin(TileServerPlugin::new("tiles.pmtiles"))
        .run()
}
```

See [`examples/`](examples/) for full runnable programs:

- `load-pbf` — parse a PBF file and print feature/index statistics
- `render-static` — render a static PNG from a PMTiles archive
- `tile-server` — serve vector tiles from a PMTiles archive
- `custom-plugin` — define and register a plugin

Example commands:

```sh
cargo run -p custom-plugin
cargo run -p load-pbf -- path/to/extract.osm.pbf
cargo run -p render-static -- path/to/archive.pmtiles out.png --bbox -122.52,37.70,-122.35,37.82
cargo run -p tile-server -- path/to/archive.pmtiles --bind 127.0.0.1:3000
```

## Building

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets
```

Minimum supported Rust version: **1.94**.

`osmic-osm` disables native PBF parsing by default at the workspace dependency
level so crates that only need the OSM data model do not pull in `osmpbf` and
`rayon`. Binaries and examples that read `.osm.pbf` files enable the
`osmic-osm/native` feature explicitly.

## License

MIT — see [LICENSE](LICENSE).
