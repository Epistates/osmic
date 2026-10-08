use std::collections::HashMap;
#[cfg(feature = "native")]
use std::env;
use std::time::Instant;

#[cfg(feature = "native")]
use rayon::prelude::*;
use tracing::info;

use osmic_core::bbox::BBox;
use osmic_core::clip::clip_geometry;
use osmic_core::error::OsmicResult;
use osmic_core::tile::{TileCoord, Zoom};
use osmic_geo::projection::bbox_to_tile_range;
use osmic_geo::simplify::simplify_geometry;
use osmic_osm::feature::Feature;
use osmic_osm::tags::TagStore;

use osmic_core::geometry::Geometry;
use osmic_osm::feature::FeatureKind;
use osmic_osm::tags::Tags;

use crate::coord::TileTransform;
use crate::encode::{TileEncoder, TileFeature};

#[cfg(feature = "native")]
use crate::sort::{ExternalFeatureSort, tile_sort_key};

/// A feature with geometry clipped to a tile bbox.
struct ClippedFeature<'a> {
    id: i64,
    kind: FeatureKind,
    geometry: Geometry,
    tags: &'a Tags,
}

impl<'a> TileFeature for ClippedFeature<'a> {
    fn id(&self) -> i64 {
        self.id
    }
    fn kind(&self) -> FeatureKind {
        self.kind
    }
    fn geometry(&self) -> &Geometry {
        &self.geometry
    }
    fn tags(&self) -> &Tags {
        self.tags
    }
}

/// Configuration for tile generation.
#[derive(Debug, Clone)]
pub struct TileGeneratorConfig {
    pub min_zoom: u8,
    pub max_zoom: u8,
    pub extent: u32,
    pub batch_size: usize,
    /// If set, switch to the streaming (external-sort) path when the estimated
    /// in-memory cost exceeds this threshold.  Estimation uses 256 bytes per
    /// feature as a conservative combined overhead for geometry, tags, and
    /// HashMap bookkeeping.
    ///
    /// `Some(0)` always uses the streaming path.
    /// `None` (default) always uses the in-memory path.
    pub max_memory_mb: Option<usize>,
}

impl Default for TileGeneratorConfig {
    fn default() -> Self {
        Self {
            min_zoom: 0,
            max_zoom: 14,
            extent: 4096,
            batch_size: 10_000,
            max_memory_mb: None,
        }
    }
}

/// Parallel tile generator.
///
/// For each zoom level, computes which tiles contain features (via feature-to-tile
/// mapping), then generates those tiles in parallel with rayon.
pub struct TileGenerator<'a> {
    features: &'a [Feature],
    tag_store: &'a TagStore,
    data_bbox: BBox,
    config: TileGeneratorConfig,
    encoder: &'a dyn TileEncoder,
}

impl<'a> TileGenerator<'a> {
    pub fn new(
        features: &'a [Feature],
        tag_store: &'a TagStore,
        data_bbox: BBox,
        config: TileGeneratorConfig,
        encoder: &'a dyn TileEncoder,
    ) -> Self {
        Self {
            features,
            tag_store,
            data_bbox,
            config,
            encoder,
        }
    }

    /// Generate all tiles and write via the callback.
    ///
    /// The callback receives `(TileCoord, &[u8])` for each generated tile.
    /// Returns the total number of tiles generated.
    ///
    /// When `config.max_memory_mb` is set and the estimated RAM required
    /// exceeds that limit, this delegates to `generate_all_streaming()`,
    /// which performs an external merge sort so only one tile's worth of
    /// features resides in memory at a time.
    pub fn generate_all<F>(&self, mut write_tile: F) -> OsmicResult<u64>
    where
        F: FnMut(TileCoord, &[u8]) -> OsmicResult<()>,
    {
        // 512 bytes per feature is a realistic upper bound for the full
        // working set: Feature struct (~96 B) + average Geometry vertex
        // payload (~200 B for mixed point/line/polygon inputs) + Tags
        // SmallVec overflow (~64 B avg with --all-tags) + HashMap<tile,
        // Vec<usize>> scatter overhead (~8 B × avg 2 tiles per feature
        // across all zoom levels ≈ 16 B). The old estimate of 256 was
        // measured on the curated-whitelist path and is now too low; a
        // US-scale run with --all-tags OOMed at 156 M features because
        // the estimate underpredicted and the streaming path didn't fire.
        const BYTES_PER_FEATURE: usize = 512;

        let estimated_mb = self.features.len().saturating_mul(BYTES_PER_FEATURE) / (1024 * 1024);

        // Auto-adapt rayon batch size to feature count. With large inputs
        // (US-scale: 150M+ features), keeping the default 10 000-tile
        // batch means up to N concurrent per-tile allocations — and each
        // tile at z4-z8 can hold 200K features × ~500 B of clipped geom,
        // so running 145 tiles in parallel adds ~14 GB of peak working
        // memory on top of the Feature vec. That's what OOM'd at z6 on
        // the US 2026-04 extract. Shrink the batch so at most a few tens
        // of tiles are in flight at once.
        let effective_batch_size = if self.features.len() > 50_000_000 {
            8
        } else if self.features.len() > 10_000_000 {
            32
        } else {
            self.config.batch_size
        };

        info!(
            features = self.features.len(),
            estimated_mb,
            max_memory_mb = ?self.config.max_memory_mb,
            configured_batch = self.config.batch_size,
            effective_batch = effective_batch_size,
            "Tile generation planning"
        );

        #[cfg(feature = "native")]
        if let Some(limit_mb) = self.config.max_memory_mb
            && estimated_mb >= limit_mb
        {
            info!(
                features = self.features.len(),
                estimated_mb,
                limit_mb,
                "Switching to streaming tile generation (estimated RAM exceeds limit)"
            );
            return self.generate_all_streaming(&mut write_tile);
        }

        let mut total_tiles = 0u64;
        let total_start = Instant::now();

        for z in self.config.min_zoom..=self.config.max_zoom {
            let zoom_start = Instant::now();

            // Step 1: Collect which tiles have features at this zoom
            let tile_features = self.collect_tile_features(z);
            let occupied_count = tile_features.len();

            if occupied_count == 0 {
                info!(zoom = z, "No tiles at this zoom, skipping");
                continue;
            }

            info!(
                zoom = z,
                occupied_tiles = occupied_count,
                "Generating tiles"
            );

            // Step 2: Generate tiles
            let tile_entries: Vec<_> = tile_features.into_iter().collect();

            for batch in tile_entries.chunks(effective_batch_size) {
                #[cfg(feature = "native")]
                let tiles: Vec<_> = batch
                    .par_iter()
                    .filter_map(|((x, y), feature_indices)| {
                        let coord = TileCoord::new(*x, *y, Zoom::new(z));
                        self.generate_single_tile(coord, feature_indices)
                            .map(|bytes| (coord, bytes))
                    })
                    .collect();

                #[cfg(not(feature = "native"))]
                let tiles: Vec<_> = batch
                    .iter()
                    .filter_map(|((x, y), feature_indices)| {
                        let coord = TileCoord::new(*x, *y, Zoom::new(z));
                        self.generate_single_tile(coord, feature_indices)
                            .map(|bytes| (coord, bytes))
                    })
                    .collect();

                for (coord, bytes) in &tiles {
                    write_tile(*coord, bytes)?;
                    total_tiles += 1;
                }
            }

            info!(
                zoom = z,
                tiles = total_tiles,
                elapsed_s = zoom_start.elapsed().as_secs_f64(),
                "Zoom level complete"
            );
        }

        info!(
            total_tiles,
            elapsed_s = total_start.elapsed().as_secs_f64(),
            "Tile generation complete"
        );

        Ok(total_tiles)
    }

    /// Build a map of (tile_x, tile_y) → [feature_indices] for a given zoom.
    fn collect_tile_features(&self, zoom: u8) -> HashMap<(u32, u32), Vec<usize>> {
        let mut tile_map: HashMap<(u32, u32), Vec<usize>> = HashMap::new();

        // Clamp to data bbox tile range to avoid iterating the whole world
        let (range_min_x, range_min_y, range_max_x, range_max_y) =
            bbox_to_tile_range(&self.data_bbox, zoom);

        for (idx, feature) in self.features.iter().enumerate() {
            if feature.kind.min_zoom() > zoom {
                continue;
            }

            let bb = feature.bbox();
            let (min_x, min_y, max_x, max_y) = bbox_to_tile_range(&bb, zoom);

            // Clamp to data bbox range
            let min_x = min_x.max(range_min_x);
            let min_y = min_y.max(range_min_y);
            let max_x = max_x.min(range_max_x);
            let max_y = max_y.min(range_max_y);

            for y in min_y..=max_y {
                for x in min_x..=max_x {
                    tile_map.entry((x, y)).or_default().push(idx);
                }
            }
        }

        tile_map
    }

    /// Streaming tile generation using an external merge sort.
    ///
    /// This path is chosen by `generate_all()` when the estimated in-memory
    /// footprint exceeds `config.max_memory_mb`.  Instead of building a full
    /// `HashMap<tile → Vec<feature_idx>>` for every zoom level, it:
    ///
    /// 1. Iterates features once per zoom level, writing `(sort_key, feature_idx)`
    ///    pairs to disk in sorted chunks (via `ExternalFeatureSort`).
    /// 2. Merges all chunks back in sort-key order using a min-heap.
    /// 3. Groups consecutive records that share the same sort key (same tile)
    ///    and calls `generate_single_tile()` for each group.
    ///
    /// Peak memory per zoom level is bounded by the sort chunk size plus the
    /// features belonging to a single tile — not the entire feature set.
    #[cfg(feature = "native")]
    pub fn generate_all_streaming<F>(&self, write_tile: &mut F) -> OsmicResult<u64>
    where
        F: FnMut(TileCoord, &[u8]) -> OsmicResult<()>,
    {
        use osmic_core::error::OsmicError;

        // Place temp files in the system temp directory.
        let tmp_dir = env::temp_dir().join("osmic_tile_sort");
        std::fs::create_dir_all(&tmp_dir).map_err(|e| {
            OsmicError::Tile(format!("Cannot create sort tmp dir {tmp_dir:?}: {e}"))
        })?;

        // Chunk size: each record is 16 bytes; target ~64 MB of RAM per chunk.
        const CHUNK_RECORDS: usize = 4_000_000; // 4 M records × 16 B = 64 MB

        let mut total_tiles = 0u64;
        let total_start = Instant::now();

        for z in self.config.min_zoom..=self.config.max_zoom {
            let zoom_start = Instant::now();

            // ── Phase 1: scatter features into the external sorter ────────────
            let mut sorter = ExternalFeatureSort::new(&tmp_dir, CHUNK_RECORDS);

            let (range_min_x, range_min_y, range_max_x, range_max_y) =
                bbox_to_tile_range(&self.data_bbox, z);

            let mut scatter_count = 0u64;
            for (idx, feature) in self.features.iter().enumerate() {
                if feature.kind.min_zoom() > z {
                    continue;
                }

                let bb = feature.bbox();
                let (min_x, min_y, max_x, max_y) = bbox_to_tile_range(&bb, z);

                let min_x = min_x.max(range_min_x);
                let min_y = min_y.max(range_min_y);
                let max_x = max_x.min(range_max_x);
                let max_y = max_y.min(range_max_y);

                for ty in min_y..=max_y {
                    for tx in min_x..=max_x {
                        let key = tile_sort_key(z, tx, ty);
                        sorter.add(key, idx).map_err(|e| {
                            OsmicError::Tile(format!("External sort write failed: {e}"))
                        })?;
                        scatter_count += 1;
                    }
                }
            }

            if scatter_count == 0 {
                info!(zoom = z, "No tiles at this zoom, skipping");
                continue;
            }

            // ── Phase 2: merge-sort and group by tile ─────────────────────────
            let sorted = sorter
                .finish()
                .map_err(|e| OsmicError::Tile(format!("External sort finish failed: {e}")))?;

            // Decode sort key back to (x, y): key layout is zoom<<48 | x<<24 | y
            let decode_xy = |key: u64| -> (u32, u32) {
                let x = ((key >> 24) & 0x00FF_FFFF) as u32;
                let y = (key & 0x00FF_FFFF) as u32;
                (x, y)
            };

            let mut current_key: Option<u64> = None;
            let mut current_indices: Vec<usize> = Vec::new();
            let mut occupied_count = 0u64;

            for (key, feat_idx) in sorted {
                if Some(key) != current_key {
                    // Flush the previous tile group.
                    if let Some(prev_key) = current_key {
                        let (x, y) = decode_xy(prev_key);
                        let coord = TileCoord::new(x, y, Zoom::new(z));
                        if let Some(bytes) = self.generate_single_tile(coord, &current_indices) {
                            write_tile(coord, &bytes)?;
                            total_tiles += 1;
                        }
                        current_indices.clear();
                    }
                    current_key = Some(key);
                    occupied_count += 1;
                }
                current_indices.push(feat_idx);
            }

            // Flush the final tile group.
            if let Some(last_key) = current_key {
                let (x, y) = decode_xy(last_key);
                let coord = TileCoord::new(x, y, Zoom::new(z));
                if let Some(bytes) = self.generate_single_tile(coord, &current_indices) {
                    write_tile(coord, &bytes)?;
                    total_tiles += 1;
                }
            }

            info!(
                zoom = z,
                occupied_tiles = occupied_count,
                tiles = total_tiles,
                elapsed_s = zoom_start.elapsed().as_secs_f64(),
                "Zoom level complete (streaming)"
            );
        }

        info!(
            total_tiles,
            elapsed_s = total_start.elapsed().as_secs_f64(),
            "Streaming tile generation complete"
        );

        Ok(total_tiles)
    }

    /// Generate a single tile from pre-computed feature indices.
    ///
    /// Clips all feature geometries to the tile bbox (with 5% buffer)
    /// before encoding to prevent "geometry exceeds extent" issues.
    fn generate_single_tile(&self, coord: TileCoord, feature_indices: &[usize]) -> Option<Vec<u8>> {
        let transform = TileTransform::new(&coord, self.config.extent);
        let tile_bbox = coord.bbox();

        // Cap features per tile to prevent pathological memory blow-up on
        // dense low-zoom tiles. At zoom 6 covering a continent, a single
        // tile can reference tens of millions of features. Clipping and
        // encoding all of them exhausts RAM and produces an unusable
        // multi-gigabyte MVT. Caps are zoom-dependent because low-zoom
        // tiles need aggressive decimation — an MVT tile over 1 MB is
        // basically unusable in any renderer regardless of source data.
        //
        // Features beyond the cap are dropped in feature-id order. A
        // future improvement would sort by an importance score (kind
        // priority + geometry area) before truncating.
        let max_per_tile = match coord.z.0 {
            0..=3 => 5_000,     // continent-level: sparse overview
            4..=6 => 20_000,    // country-level: major roads + large areas
            7..=9 => 60_000,    // region-level: detailed road network
            10..=12 => 150_000, // metro-level: full detail
            _ => 500_000,       // city / block level: cap relaxed
        };
        let use_indices = if feature_indices.len() > max_per_tile {
            &feature_indices[..max_per_tile]
        } else {
            feature_indices
        };

        // Simplify, clip, and group features by layer
        let zoom = coord.z.0;
        let mut layer_map: HashMap<&str, Vec<ClippedFeature>> = HashMap::new();
        for &idx in use_indices {
            let feature = &self.features[idx];
            // Simplify geometry for current zoom level (reduces vertex count)
            let simplified = simplify_geometry(&feature.geometry, zoom);
            // Clip to tile bbox with 5% buffer for anti-aliasing
            if let Some(clipped_geom) = clip_geometry(&simplified, &tile_bbox, 0.05) {
                let layer_name = feature.kind.layer_name();
                layer_map
                    .entry(layer_name)
                    .or_default()
                    .push(ClippedFeature {
                        id: feature.id,
                        kind: feature.kind,
                        geometry: clipped_geom,
                        tags: &feature.tags,
                    });
            }
        }

        // Build layer entries with trait object references for the encoder
        let layer_entries: Vec<(&str, Vec<&dyn TileFeature>)> = layer_map
            .iter()
            .map(|(name, features)| {
                let refs: Vec<&dyn TileFeature> =
                    features.iter().map(|f| f as &dyn TileFeature).collect();
                (*name, refs)
            })
            .collect();

        self.encoder.encode_clipped(
            self.config.extent,
            &transform,
            &layer_entries,
            self.tag_store,
        )
    }
}
