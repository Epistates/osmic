//! PMTiles archive writer — native only (requires pmtiles with file I/O).
#![cfg(feature = "native")]

use std::fs::File;
use std::path::Path;

use pmtiles::{Compression, PmTilesStreamWriter, PmTilesWriter, TileType};
use tracing::info;

use osmic_core::bbox::BBox;
use osmic_core::error::{OsmicError, OsmicResult};
use osmic_core::tile::TileCoord;

/// Wrapper around PMTiles stream writer for tile archive creation.
pub struct PmTilesArchive {
    writer: PmTilesStreamWriter<File>,
}

impl PmTilesArchive {
    /// Create a new PMTiles archive at the given path.
    pub fn create(
        path: &Path,
        bbox: &BBox,
        min_zoom: u8,
        max_zoom: u8,
        tile_type: TileType,
    ) -> OsmicResult<Self> {
        info!(path = %path.display(), "Creating PMTiles archive");

        let file = File::create(path)?;
        let center = bbox.center();

        let metadata = build_metadata(min_zoom, max_zoom);

        let writer = PmTilesWriter::new(tile_type)
            .tile_compression(Compression::Gzip)
            .internal_compression(Compression::Gzip)
            .min_zoom(min_zoom)
            .max_zoom(max_zoom)
            .bounds(bbox.min_lon, bbox.min_lat, bbox.max_lon, bbox.max_lat)
            .center(center.lon, center.lat)
            .center_zoom(min_zoom.saturating_add(max_zoom) / 2)
            .metadata(&metadata)
            .create(file)
            .map_err(|e| OsmicError::Tile(format!("Failed to create PMTiles: {e}")))?;

        Ok(Self { writer })
    }

    /// Add a tile to the archive.
    pub fn add_tile(&mut self, coord: TileCoord, data: &[u8]) -> OsmicResult<()> {
        let pm_coord = pmtiles::TileCoord::new(coord.z.0, coord.x, coord.y)
            .map_err(|e| OsmicError::Tile(format!("Invalid tile coord {coord}: {e}")))?;

        self.writer
            .add_tile(pm_coord, data)
            .map_err(|e| OsmicError::Tile(format!("Failed to write tile {coord}: {e}")))?;

        Ok(())
    }

    /// Finalize the archive, writing the header and directory.
    pub fn finalize(self) -> OsmicResult<()> {
        self.writer
            .finalize()
            .map_err(|e| OsmicError::Tile(format!("Failed to finalize PMTiles: {e}")))?;

        info!("PMTiles archive finalized");
        Ok(())
    }
}

/// Build TileJSON-compatible metadata for the PMTiles archive.
///
/// The `vector_layers` array is consumed by downstream tile servers like
/// [Martin](https://github.com/maplibre/martin) to advertise which layers
/// and attributes are available. We emit the full 19-layer inventory so
/// clients can introspect the schema via Martin's `/catalog` endpoint
/// even if the current tile gen run was filtered to a subset.
///
/// The per-layer `minzoom` values mirror `osmic_osm::feature::min_tile_zoom`.
/// The `fields` dictionary includes every key the MVT encoder knows about
/// (both the curated whitelist and the common tags exposed in `--all-tags`
/// mode). Martin does not require every field to actually appear on every
/// feature — it uses the schema as a hint for clients.
fn build_metadata(_min_zoom: u8, max_zoom: u8) -> String {
    // (id, human description, per-layer minzoom). minzooms mirror
    // `FeatureKind::min_tile_zoom` in `osmic-osm/src/feature.rs`.
    const LAYER_SPEC: &[(&str, &str, u8)] = &[
        ("highway", "Road network", 4),
        ("building", "Buildings", 13),
        ("water", "Water features", 0),
        ("landuse", "Land use areas", 7),
        ("natural", "Natural features", 0),
        ("railway", "Railway lines", 8),
        ("amenity", "Amenity points and areas", 13),
        ("leisure", "Leisure areas", 8),
        ("boundary", "Administrative boundaries", 2),
        ("place", "Place labels", 4),
        ("shop", "Retail points", 14),
        ("tourism", "Tourism points", 13),
        ("office", "Office points", 14),
        ("healthcare", "Healthcare points", 14),
        ("craft", "Craft / trade points", 14),
        ("historic", "Historic points", 13),
        ("club", "Club points", 14),
        ("emergency", "Emergency services", 13),
        ("education", "Education points", 13),
    ];

    // Every tag key the MVT encoder can emit — the curated whitelist
    // plus the pass-through keys most commonly present on POI features.
    // Martin clients use this to populate schema introspection UIs.
    let fields = serde_json::json!({
        "class":            "String",
        "name":             "String",
        "addr:street":      "String",
        "addr:housenumber": "String",
        "addr:city":        "String",
        "addr:postcode":    "String",
        "phone":            "String",
        "contact:phone":    "String",
        "website":          "String",
        "contact:website":  "String",
        "opening_hours":    "String",
        "cuisine":          "String",
        "brand":            "String",
        "operator":         "String",
        "description":      "String",
        "shop":             "String",
        "amenity":          "String",
        "tourism":          "String",
        "office":           "String",
        "craft":            "String",
        "healthcare":       "String",
        "historic":         "String",
    });

    let layers: Vec<serde_json::Value> = LAYER_SPEC
        .iter()
        .map(|(id, desc, layer_min)| {
            serde_json::json!({
                "id": id,
                "description": desc,
                "fields": fields,
                "minzoom": layer_min,
                "maxzoom": max_zoom,
            })
        })
        .collect();

    serde_json::json!({
        "vector_layers": layers,
        "name": "Osmic",
        "description": "Generated by Osmic",
        "attribution": "OpenStreetMap contributors",
        "type": "baselayer",
        "format": "pbf",
        "version": "2",
    })
    .to_string()
}
