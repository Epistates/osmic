//! Tile generation: features → rendered pieces → external sort → tiles.
//!
//! [`TileGenerator`] implements [`FeatureSink`], so the PBF pipeline can
//! stream features straight into it: every feature is rendered for every
//! zoom and tile as it arrives (in parallel, on the PBF worker threads) and
//! the pieces go to an [`ExternalSorter`] keyed by Hilbert tile id. Memory is
//! bounded by the sort budget, not by the number of features.
//!
//! [`TileGenerator::finish`] then reads the sorted pieces back in
//! key-range partitions: the rayon pool groups, encodes and compresses the
//! tiles of several partitions at once while the calling thread delivers
//! them in tile-id order — so archives are clustered and byte-for-byte
//! reproducible.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Instant;

use rayon::prelude::*;
use tracing::{info, warn};

use osmic_core::BBox;
use osmic_osm::{Feature, FeatureSink, TagStore};

use crate::assemble::{AssembledTile, TileCompression, assemble, secondary_key};
use crate::encode::TileEncoder;
use crate::error::TileError;
use crate::model::TileFeature;
use crate::pmtiles::{
    ArchiveInfo, ArchiveOptions, LayerStats, PmTilesArchive, metadata_json, tile_coord, tile_id,
};
use crate::render::{RenderConfig, Renderer};
use crate::sorter::{ExternalSorter, SortedRuns};

/// Tile generation settings.
#[derive(Debug, Clone)]
pub struct TileGeneratorConfig {
    pub render: RenderConfig,
    /// Compressed size budget per tile; least important features are
    /// dropped from tiles that exceed it.
    pub max_tile_bytes: usize,
    pub compression: TileCompression,
    /// Memory for buffering rendered pieces before spilling to disk.
    pub memory_budget: usize,
    /// Parent directory for temporary sort files (system temp if `None`).
    pub temp_dir: Option<PathBuf>,
}

impl Default for TileGeneratorConfig {
    fn default() -> Self {
        Self {
            render: RenderConfig::default(),
            max_tile_bytes: 500_000,
            compression: TileCompression::Gzip,
            memory_budget: 4 << 30,
            temp_dir: None,
        }
    }
}

impl TileGeneratorConfig {
    fn validate(&self) -> Result<(), TileError> {
        let r = &self.render;
        let bad = |m: String| Err(TileError::Config(m));
        if r.min_zoom > r.max_zoom {
            return bad(format!(
                "min zoom {} exceeds max zoom {}",
                r.min_zoom, r.max_zoom
            ));
        }
        if r.max_zoom > osmic_core::Zoom::MAX.0 {
            return bad(format!(
                "max zoom {} exceeds {}",
                r.max_zoom,
                osmic_core::Zoom::MAX.0
            ));
        }
        if r.extent == 0 || r.extent > 1 << 16 {
            return bad(format!("extent {} must be in 1..=65536", r.extent));
        }
        for (name, v) in [
            ("buffer", r.buffer_px),
            ("simplify tolerance", r.simplify_px),
            ("max-zoom simplify tolerance", r.simplify_px_max_zoom),
            ("minimum feature size", r.min_size_px),
        ] {
            if !v.is_finite() || v < 0.0 {
                return bad(format!("{name} must be a non-negative number, got {v}"));
            }
        }
        if r.buffer_px > 128.0 {
            return bad(format!("buffer {} px exceeds half a tile", r.buffer_px));
        }
        if self.max_tile_bytes == 0 {
            return bad("max tile bytes must be positive".into());
        }
        Ok(())
    }
}

/// Statistics collected while rendering.
#[derive(Debug, Clone)]
struct RenderStats {
    layers: BTreeMap<String, LayerStats>,
    bbox: BBox,
    features: u64,
    min_zoom: Option<u8>,
    max_zoom: Option<u8>,
}

impl Default for RenderStats {
    fn default() -> Self {
        Self {
            layers: BTreeMap::new(),
            bbox: BBox::empty(),
            features: 0,
            min_zoom: None,
            max_zoom: None,
        }
    }
}

impl RenderStats {
    fn note_zoom(&mut self, z: u8) {
        self.min_zoom = Some(self.min_zoom.map_or(z, |m| m.min(z)));
        self.max_zoom = Some(self.max_zoom.map_or(z, |m| m.max(z)));
    }

    fn merge(&mut self, other: RenderStats) {
        for (k, v) in &other.layers {
            self.layers.entry(k.clone()).or_default().merge(v);
        }
        self.bbox.extend(&other.bbox);
        self.features += other.features;
        for z in other.min_zoom.into_iter().chain(other.max_zoom) {
            self.note_zoom(z);
        }
    }
}

/// Summary of a finished run.
#[derive(Debug, Clone, Default)]
pub struct TileSummary {
    /// Features rendered (before slicing into tiles).
    pub input_features: u64,
    pub tiles: u64,
    /// Feature pieces written into tiles.
    pub features: u64,
    /// Feature pieces dropped to meet the tile size budget.
    pub dropped_features: u64,
    /// Tiles that hit the size budget.
    pub budget_limited_tiles: u64,
    pub largest_tile_bytes: usize,
    pub total_bytes: u64,
    /// Tiles per zoom level.
    pub tiles_per_zoom: BTreeMap<u8, u64>,
    /// Bytes spilled to temporary files by the external sort.
    pub spilled_bytes: u64,
    /// Time from generator creation to the start of the merge.
    pub render_seconds: f64,
    pub encode_seconds: f64,
}

impl TileSummary {
    fn record(&mut self, t: &AssembledTile) {
        self.tiles += 1;
        self.features += t.features as u64;
        self.dropped_features += t.dropped as u64;
        self.budget_limited_tiles += u64::from(t.dropped > 0);
        self.largest_tile_bytes = self.largest_tile_bytes.max(t.data.len());
        self.total_bytes += t.data.len() as u64;
        *self.tiles_per_zoom.entry(t.coord.z.0).or_default() += 1;
    }

    /// Lowest and highest zoom with at least one tile.
    pub fn zoom_range(&self) -> Option<(u8, u8)> {
        Some((
            *self.tiles_per_zoom.keys().next()?,
            *self.tiles_per_zoom.keys().last()?,
        ))
    }
}

/// What every partition worker needs to turn records into tiles.
struct EncodeContext<'a> {
    extent: u32,
    encoder: &'a dyn TileEncoder,
    compression: TileCompression,
    max_bytes: usize,
}

/// Assemble every tile of partition `i`, in order, skipping empty tiles.
fn encode_partition(
    runs: &SortedRuns,
    i: usize,
    ctx: &EncodeContext<'_>,
) -> Result<Vec<AssembledTile>, TileError> {
    let part = runs.partition(i)?;
    let mut tiles = Vec::new();
    for (key, records) in part.groups() {
        let features = records
            .map(|(secondary, payload)| Ok((secondary, TileFeature::decode(payload)?)))
            .collect::<Result<Vec<_>, TileError>>()?;
        let tile = assemble(
            tile_coord(key)?,
            features,
            ctx.extent,
            ctx.encoder,
            ctx.compression,
            ctx.max_bytes,
        )?;
        if !tile.data.is_empty() {
            tiles.push(tile);
        }
    }
    Ok(tiles)
}

/// Streams features into tiles. See the module docs.
pub struct TileGenerator {
    config: TileGeneratorConfig,
    encoder: Box<dyn TileEncoder>,
    tag_store: Arc<TagStore>,
    sorter: ExternalSorter,
    stats: Mutex<RenderStats>,
    started: Instant,
}

fn poisoned<T>(_: T) -> TileError {
    TileError::Config("render statistics lock poisoned".into())
}

impl TileGenerator {
    /// `tag_store` must be the store the features' tags were interned in.
    pub fn new(
        config: TileGeneratorConfig,
        encoder: Box<dyn TileEncoder>,
        tag_store: Arc<TagStore>,
    ) -> Result<Self, TileError> {
        config.validate()?;
        let sorter = ExternalSorter::new(config.temp_dir.as_deref(), config.memory_budget)?;
        Ok(Self {
            config,
            encoder,
            tag_store,
            sorter,
            stats: Mutex::new(RenderStats::default()),
            started: Instant::now(),
        })
    }

    pub fn config(&self) -> &TileGeneratorConfig {
        &self.config
    }

    /// Render `features` into the sort buffer. Safe to call from many
    /// threads at once.
    pub fn add(&self, features: &[Feature]) -> Result<(), TileError> {
        let renderer = Renderer::new(&self.config.render, &self.tag_store);
        let mut writer = self.sorter.writer();
        let mut local = RenderStats::default();
        let mut failure: Option<TileError> = None;
        for feature in features {
            local.features += 1;
            local.bbox.extend(&feature.bbox());
            renderer.render(feature, &mut |piece| {
                if failure.is_some() {
                    return;
                }
                let key = match tile_id(piece.tile) {
                    Ok(k) => k,
                    Err(e) => {
                        failure = Some(e);
                        return;
                    }
                };
                let z = piece.tile.z.0;
                local.note_zoom(z);
                local
                    .layers
                    .entry(piece.layer.as_str().to_string())
                    .or_default()
                    .record(z, piece.feature.attributes.iter().map(|(k, _)| k));
                let secondary = secondary_key(piece.layer, piece.importance, piece.size_class);
                if let Err(e) = writer.push_with(key, secondary, |buf| piece.feature.encode(buf)) {
                    failure = Some(e.into());
                }
            });
            if let Some(e) = failure.take() {
                return Err(e);
            }
        }
        self.stats.lock().map_err(poisoned)?.merge(local);
        Ok(())
    }

    /// Render an in-memory feature slice using all cores.
    pub fn add_parallel(&self, features: &[Feature]) -> Result<(), TileError> {
        features.par_chunks(1024).try_for_each(|c| self.add(c))
    }

    fn stats(&self) -> Result<RenderStats, TileError> {
        Ok(self.stats.lock().map_err(poisoned)?.clone())
    }

    /// Bounds of every feature added so far.
    pub fn bbox(&self) -> Result<BBox, TileError> {
        Ok(self.stats()?.bbox)
    }

    /// Merge, encode and deliver every tile in tile-id order to `write`.
    pub fn finish(
        self,
        mut write: impl FnMut(&AssembledTile) -> Result<(), TileError>,
    ) -> Result<TileSummary, TileError> {
        let render_seconds = self.started.elapsed().as_secs_f64();
        let encode_start = Instant::now();
        let mut summary = TileSummary {
            input_features: self.stats()?.features,
            spilled_bytes: self.sorter.spilled_bytes(),
            render_seconds,
            ..Default::default()
        };
        info!(
            pieces = self.sorter.records(),
            spilled_mib = self.sorter.spilled_bytes() >> 20,
            secs = render_seconds,
            "Rendering complete; merging tiles"
        );
        let runs = self.sorter.finish()?;
        let (parts, window) = (runs.partition_count(), runs.window());
        let ctx = EncodeContext {
            extent: self.config.render.extent,
            encoder: self.encoder.as_ref(),
            compression: self.config.compression,
            max_bytes: self.config.max_tile_bytes,
        };
        let max_bytes = ctx.max_bytes;

        // Partitions are encoded on the rayon pool, at most `window` at a
        // time, and written here strictly in order.
        let abort = AtomicBool::new(false);
        let (tx, rx) = mpsc::channel::<(usize, Result<Vec<AssembledTile>, TileError>)>();
        rayon::in_place_scope(|scope| -> Result<(), TileError> {
            let mut pending = BTreeMap::new();
            let (mut spawned, mut written) = (0usize, 0usize);
            let result = (|| {
                while written < parts {
                    while spawned < parts && spawned - written < window {
                        let (tx, runs, ctx, abort) = (tx.clone(), &runs, &ctx, &abort);
                        let i = spawned;
                        scope.spawn(move |_| {
                            if !abort.load(Ordering::Relaxed) {
                                let _ = tx.send((i, encode_partition(runs, i, ctx)));
                            }
                        });
                        spawned += 1;
                    }
                    let (i, tiles) = rx
                        .recv()
                        .map_err(|_| TileError::Config("tile encoder stopped".into()))?;
                    pending.insert(i, tiles);
                    while let Some(tiles) = pending.remove(&written) {
                        for t in &tiles? {
                            summary.record(t);
                            write(t)?;
                        }
                        written += 1;
                    }
                }
                Ok(())
            })();
            if result.is_err() {
                abort.store(true, Ordering::Relaxed);
            }
            result
        })?;
        summary.encode_seconds = encode_start.elapsed().as_secs_f64();
        if summary.budget_limited_tiles > 0 {
            warn!(
                tiles = summary.budget_limited_tiles,
                dropped_features = summary.dropped_features,
                max_tile_bytes = max_bytes,
                "Tiles over the size budget had their least important features dropped"
            );
        }
        info!(
            tiles = summary.tiles,
            bytes = summary.total_bytes,
            largest = summary.largest_tile_bytes,
            secs = summary.encode_seconds,
            "Tiles encoded"
        );
        Ok(summary)
    }

    /// Finish and write a PMTiles archive to `path` (atomically).
    pub fn write_pmtiles(
        self,
        path: &Path,
        info: &ArchiveInfo,
        overwrite: bool,
    ) -> Result<TileSummary, TileError> {
        let stats = self.stats()?;
        let bounds = if stats.bbox.is_valid() {
            let m = osmic_core::mercator::MAX_LATITUDE;
            BBox::new(
                stats.bbox.min_lon.max(-180.0),
                stats.bbox.min_lat.max(-m),
                stats.bbox.max_lon.min(180.0),
                stats.bbox.max_lat.min(m),
            )
        } else {
            BBox::world()
        };
        let render = &self.config.render;
        let options = ArchiveOptions {
            format: self.encoder.format(),
            compression: self.config.compression,
            bounds,
            min_zoom: stats.min_zoom.unwrap_or(render.min_zoom),
            max_zoom: stats.max_zoom.unwrap_or(render.max_zoom),
            metadata: metadata_json(info, self.encoder.format(), &stats.layers),
            overwrite,
        };
        let mut archive = PmTilesArchive::create(path, &options)?;
        let summary = self.finish(|t| archive.add_tile(t.coord, &t.data))?;
        archive.finalize()?;
        Ok(summary)
    }
}

impl FeatureSink for TileGenerator {
    fn accept(
        &self,
        features: Vec<Feature>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Ok(self.add(&features)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::MvtEncoder;
    use geo_types::{LineString, Point};
    use osmic_core::{Geometry, OsmId};
    use osmic_osm::TagRetention;
    use osmic_osm::feature::{AmenityKind, FeatureKind, HighwayKind};

    fn features(store: &TagStore) -> Vec<Feature> {
        let mut out = Vec::new();
        for i in 0..500 {
            let lon = -122.5 + f64::from(i) * 0.001;
            out.push(Feature {
                id: OsmId::node(i64::from(i)),
                kind: FeatureKind::Amenity(AmenityKind::Cafe),
                geometry: Geometry::Point(Point::new(lon, 37.77)),
                tags: store.intern_tags([("amenity", "cafe"), ("name", "c")], &TagRetention::All),
            });
        }
        out.push(Feature {
            id: OsmId::way(1),
            kind: FeatureKind::Highway(HighwayKind::Motorway),
            geometry: Geometry::Line(LineString::from(vec![(-123.0, 37.0), (-121.0, 38.5)])),
            tags: store.intern_tags([("highway", "motorway")], &TagRetention::All),
        });
        out
    }

    fn generate(dir: &Path, name: &str) -> (PathBuf, TileSummary) {
        let store = Arc::new(TagStore::new());
        let config = TileGeneratorConfig {
            temp_dir: Some(dir.to_path_buf()),
            ..Default::default()
        };
        let g = TileGenerator::new(config, Box::new(MvtEncoder), Arc::clone(&store)).expect("new");
        g.add_parallel(&features(&store)).expect("add");
        let path = dir.join(name);
        let summary = g
            .write_pmtiles(&path, &ArchiveInfo::default(), false)
            .expect("write");
        (path, summary)
    }

    #[test]
    fn archive_is_reproducible_clustered_and_complete() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (a, sa) = generate(dir.path(), "a.pmtiles");
        let (b, _) = generate(dir.path(), "b.pmtiles");
        assert_eq!(
            std::fs::read(&a).expect("a"),
            std::fs::read(&b).expect("b"),
            "deterministic"
        );
        assert!(sa.tiles > 0);
        assert_eq!(
            sa.zoom_range(),
            Some((4, 14)),
            "motorway from z4, cafés at z13-14"
        );
        assert_eq!(sa.dropped_features, 0);
        assert_eq!(sa.input_features, 501);

        let bytes = std::fs::read(&a).expect("read");
        let header = pmtiles::Header::try_from_bytes(bytes::Bytes::from(bytes)).expect("header");
        assert!(header.clustered(), "tiles written in Hilbert order");
        assert_eq!((header.min_zoom, header.max_zoom), (4, 14));
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read")
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with('.'))
            .collect();
        assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");
    }

    #[test]
    fn invalid_configs_are_rejected() {
        let store = Arc::new(TagStore::new());
        let mut c = TileGeneratorConfig::default();
        c.render.min_zoom = 10;
        c.render.max_zoom = 5;
        assert!(TileGenerator::new(c, Box::new(MvtEncoder), Arc::clone(&store)).is_err());
        let mut c = TileGeneratorConfig::default();
        c.render.buffer_px = f64::NAN;
        assert!(TileGenerator::new(c, Box::new(MvtEncoder), store).is_err());
    }
}
