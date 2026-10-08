//! Tile generation: features → rendered pieces → external sort → tiles.
//!
//! [`TileGenerator`] implements [`FeatureSink`], so the PBF pipeline can
//! stream features straight into it: every feature is rendered for every
//! zoom and tile as it arrives (in parallel, on the PBF worker threads) and
//! the pieces go to a parallel external sort keyed by Hilbert tile id.
//! Memory is bounded by the sort budget, not by the number of features.
//!
//! [`TileGenerator::finish`] then reads the sorted pieces back in
//! key-range partitions: the rayon pool groups, encodes and compresses the
//! tiles of several partitions at once while the calling thread delivers
//! them in tile-id order — so archives are clustered and byte-for-byte
//! reproducible.

use std::collections::BTreeMap;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use rayon::prelude::*;
use tracing::{info, warn};

use osmic_core::BBox;
use osmic_osm::{Feature, FeatureSink, Layer, TagStore};

use crate::assemble::{AssembledTile, TileCompression, assemble, secondary_key};
use crate::encode::TileEncoder;
use crate::error::TileError;
use crate::pmtiles::{
    ArchiveInfo, ArchiveOptions, LayerStats, PmTilesArchive, metadata_json, tile_coord, tile_id,
};
use crate::record;
use crate::render::{RenderConfig, Renderer};
use crate::sorter::{ExternalSorter, SortedRuns};

/// Tile generation settings.
///
/// Start from [`TileGeneratorConfig::default`] and set the fields to change;
/// new fields may be added in minor releases.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct TileGeneratorConfig {
    /// How features are rendered into tiles.
    pub render: RenderConfig,
    /// Compressed size budget per tile; least important features are
    /// dropped from tiles that exceed it.
    pub max_tile_bytes: usize,
    /// Compression applied to each tile.
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
        if r.max_zoom > osmic_core::Zoom::MAX.get() {
            return bad(format!(
                "max zoom {} exceeds {}",
                r.max_zoom,
                osmic_core::Zoom::MAX.get()
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
    /// Indexed by `Layer as usize` (the order of [`Layer::ALL`]).
    layers: [LayerStats; Layer::ALL.len()],
    bbox: BBox,
    features: u64,
    min_zoom: Option<u8>,
    max_zoom: Option<u8>,
}

impl Default for RenderStats {
    fn default() -> Self {
        Self {
            layers: Default::default(),
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
        for (mine, theirs) in self.layers.iter_mut().zip(&other.layers) {
            mine.merge(theirs);
        }
        self.bbox.extend(&other.bbox);
        self.features += other.features;
        for z in other.min_zoom.into_iter().chain(other.max_zoom) {
            self.note_zoom(z);
        }
    }

    /// Per-layer statistics keyed by layer name, as archive metadata wants.
    fn layers_by_name(&self) -> BTreeMap<String, LayerStats> {
        Layer::ALL
            .iter()
            .zip(&self.layers)
            .filter(|(_, s)| s.features > 0)
            .map(|(l, s)| (l.as_str().to_string(), s.clone()))
            .collect()
    }
}

/// Summary of a finished run.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct TileSummary {
    /// Features rendered (before slicing into tiles).
    pub input_features: u64,
    /// Non-empty tiles written.
    pub tiles: u64,
    /// Feature pieces written into tiles.
    pub features: u64,
    /// Feature pieces dropped to meet the tile size budget.
    pub dropped_features: u64,
    /// Tiles that hit the size budget.
    pub budget_limited_tiles: u64,
    /// Size of the largest tile, compressed.
    pub largest_tile_bytes: usize,
    /// Size of all tiles, compressed.
    pub total_bytes: u64,
    /// Tiles per zoom level.
    pub tiles_per_zoom: BTreeMap<u8, u64>,
    /// Bytes spilled to temporary files by the external sort.
    pub spilled_bytes: u64,
    /// Time from generator creation to the start of the merge.
    pub render_seconds: f64,
    /// Time spent merging, encoding and writing tiles.
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
        *self.tiles_per_zoom.entry(t.coord.z.get()).or_default() += 1;
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
            .map(|(secondary, payload)| Ok((secondary, record::decode(payload)?)))
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

/// Wait for a message from jobs running on the rayon pool without ever
/// parking a pool thread while there is pool work it could run.
///
/// On a pool thread, queued jobs (possibly the very ones being waited for)
/// are run here; only when there is nothing to run does the thread sleep, and
/// then briefly, to look for new work again. Off the pool, it simply blocks.
fn recv_helping<M>(rx: &mpsc::Receiver<M>) -> Result<M, mpsc::RecvError> {
    loop {
        match rx.try_recv() {
            Ok(m) => return Ok(m),
            Err(mpsc::TryRecvError::Disconnected) => return Err(mpsc::RecvError),
            Err(mpsc::TryRecvError::Empty) => {}
        }
        match rayon::yield_now() {
            Some(rayon::Yield::Executed) => {}
            Some(rayon::Yield::Idle) => match rx.recv_timeout(Duration::from_millis(1)) {
                Ok(m) => return Ok(m),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return Err(mpsc::RecvError),
            },
            None => return rx.recv(),
        }
    }
}

/// Encode partitions `0..parts` with `encode` on the rayon pool, at most
/// `window` at a time, and hand every item to `deliver` strictly in
/// partition order on the calling thread.
///
/// Encoding is a sliding window: a new partition starts as soon as the
/// oldest finished one is delivered, so threads never wait for a whole
/// batch. Waiting for results goes through [`recv_helping`], which makes this
/// safe to call from inside a rayon pool of any size, including a single
/// thread. A panic in `encode` is re-raised here.
///
/// After an error, partitions not yet started are skipped and the error is
/// returned.
fn encode_in_order<T: Send, E: Send>(
    parts: usize,
    window: usize,
    encode: impl Fn(usize) -> Result<Vec<T>, E> + Sync,
    mut deliver: impl FnMut(T) -> Result<(), E>,
) -> Result<(), E> {
    let window = window.max(1);
    let abort = AtomicBool::new(false);
    let (tx, rx) = mpsc::channel::<(usize, std::thread::Result<Result<Vec<T>, E>>)>();
    let (encode, abort_ref) = (&encode, &abort);
    rayon::in_place_scope(|scope| {
        let mut pending = BTreeMap::new();
        let (mut spawned, mut delivered) = (0usize, 0usize);
        let mut run = || -> Result<(), E> {
            while delivered < parts {
                while spawned < parts && spawned - delivered < window {
                    let (tx, i) = (tx.clone(), spawned);
                    scope.spawn(move |_| {
                        if !abort_ref.load(Ordering::Relaxed) {
                            let result = panic::catch_unwind(AssertUnwindSafe(|| encode(i)));
                            let _ = tx.send((i, result));
                        }
                    });
                    spawned += 1;
                }
                // The sender held here keeps the channel open, so this only
                // fails if that invariant is broken.
                let Ok((i, result)) = recv_helping(&rx) else {
                    unreachable!("partition channel closed while a sender is held");
                };
                pending.insert(i, result);
                while let Some(result) = pending.remove(&delivered) {
                    let items = result.unwrap_or_else(|p| {
                        abort_ref.store(true, Ordering::Relaxed);
                        panic::resume_unwind(p)
                    })?;
                    items.into_iter().try_for_each(&mut deliver)?;
                    delivered += 1;
                }
            }
            Ok(())
        };
        let result = run();
        if result.is_err() {
            abort.store(true, Ordering::Relaxed);
        }
        result
    })
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

    /// The settings this generator was created with.
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
        // A feature's attributes, encoded once and appended to the record
        // of each of its pieces.
        let mut attributes = Vec::new();
        for feature in features {
            local.features += 1;
            local.bbox.extend(&feature.bbox());
            let mut first_piece = true;
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
                let z = piece.tile.z.get();
                local.note_zoom(z);
                let layer = &mut local.layers[piece.layer as usize];
                layer.record_piece(z);
                if first_piece {
                    first_piece = false;
                    layer.record_fields(piece.attributes.iter().map(|(k, _)| k));
                    attributes.clear();
                    record::encode_attributes(piece.attributes, &mut attributes);
                }
                let secondary = secondary_key(piece.layer, piece.importance, piece.size_class);
                let pushed = writer.push_with(key, secondary, |buf| {
                    record::encode_geometry(piece.id, piece.geom_type, &piece.parts, buf);
                    buf.extend_from_slice(&attributes);
                });
                if let Err(e) = pushed {
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

        encode_in_order(
            parts,
            window,
            |i| encode_partition(&runs, i, &ctx),
            |t| {
                summary.record(&t);
                write(&t)
            },
        )?;
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
            metadata: metadata_json(info, self.encoder.format(), &stats.layers_by_name()),
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
    use crate::encode::{MvtEncoder, TileFormat};
    use crate::render::AttributeMode;
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

    /// A deterministic, varied feature set: buildings (some with holes),
    /// named roads crossing many tiles, POIs, large landuse and water
    /// polygons, a boundary and a railway — with curated and extra tags.
    fn mixed_features(store: &TagStore) -> Vec<Feature> {
        use geo_types::{MultiPolygon, Polygon};
        use osmic_osm::feature::{
            BoundaryKind, BuildingKind, LanduseKind, NaturalKind, RailwayKind, ShopKind, WaterKind,
        };
        let mut s = 0x9E37_79B9_7F4A_7C15u64;
        let mut rand = move || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            (s >> 11) as f64 / (1u64 << 53) as f64
        };
        let square = |x: f64, y: f64, d: f64| {
            LineString::from(vec![(x, y), (x + d, y), (x + d, y + d), (x, y + d), (x, y)])
        };
        let hole = |x: f64, y: f64, d: f64| {
            LineString::from(vec![(x, y), (x, y + d), (x + d, y + d), (x + d, y), (x, y)])
        };
        let tags = |t: &[(&str, &str)]| store.intern_tags(t.iter().copied(), &TagRetention::All);
        let mut out = Vec::new();
        for i in 0..3_000i64 {
            let (x, y) = (-122.52 + rand() * 0.2, 37.70 + rand() * 0.12);
            let d = 0.0001 + rand() * 0.0004;
            let interiors = if i % 7 == 0 {
                vec![hole(x + d / 4.0, y + d / 4.0, d / 2.0)]
            } else {
                vec![]
            };
            let name = format!("Building {i}");
            let number = (i % 300).to_string();
            let mut t = vec![("building", "yes"), ("addr:housenumber", number.as_str())];
            if i % 3 == 0 {
                t.push(("name", &name));
            }
            if i % 5 == 0 {
                t.push(("fixme", "check"));
            }
            out.push(Feature {
                id: OsmId::way(i),
                kind: FeatureKind::Building(BuildingKind::Yes),
                geometry: Geometry::Polygon(Polygon::new(square(x, y, d), interiors)),
                tags: tags(&t),
            });
        }
        let kinds = [
            (HighwayKind::Motorway, "motorway"),
            (HighwayKind::Primary, "primary"),
            (HighwayKind::Residential, "residential"),
            (HighwayKind::Footway, "footway"),
        ];
        for i in 0..400i64 {
            let (kind, value) = kinds[(i % 4) as usize];
            let (mut x, mut y) = (-122.6 + rand() * 0.4, 37.6 + rand() * 0.3);
            let mut pts = vec![(x, y)];
            for _ in 0..(3 + i % 20) {
                x += (rand() - 0.5) * 0.05;
                y += (rand() - 0.5) * 0.05;
                pts.push((x, y));
            }
            let name = format!("Street {}", i % 37);
            let reference = format!("R{}", i % 11);
            out.push(Feature {
                id: OsmId::way(10_000 + i),
                kind: FeatureKind::Highway(kind),
                geometry: Geometry::Line(LineString::from(pts)),
                tags: tags(&[
                    ("highway", value),
                    ("name", &name),
                    ("ref", &reference),
                    ("surface", "asphalt"),
                ]),
            });
        }
        for i in 0..500i64 {
            let (x, y) = (-122.5 + rand() * 0.15, 37.72 + rand() * 0.1);
            let name = format!("Place {i}");
            let (kind, t) = if i % 2 == 0 {
                (
                    FeatureKind::Amenity(AmenityKind::Cafe),
                    vec![
                        ("amenity", "cafe"),
                        ("name", name.as_str()),
                        ("cuisine", "coffee"),
                    ],
                )
            } else {
                (
                    FeatureKind::Shop(ShopKind::Supermarket),
                    vec![
                        ("shop", "supermarket"),
                        ("name", name.as_str()),
                        ("opening_hours", "24/7"),
                    ],
                )
            };
            out.push(Feature {
                id: OsmId::node(i),
                kind,
                geometry: Geometry::Point(Point::new(x, y)),
                tags: tags(&t),
            });
        }
        out.push(Feature {
            id: OsmId::relation(1),
            kind: FeatureKind::Landuse(LanduseKind::Forest),
            geometry: Geometry::Polygon(Polygon::new(
                square(-122.55, 37.65, 0.3),
                vec![hole(-122.5, 37.7, 0.1)],
            )),
            tags: tags(&[("landuse", "forest"), ("name", "Big Wood")]),
        });
        out.push(Feature {
            id: OsmId::relation(2),
            kind: FeatureKind::Water(WaterKind::Lake),
            geometry: Geometry::MultiPolygon(MultiPolygon(vec![
                Polygon::new(square(-122.45, 37.75, 0.05), vec![]),
                Polygon::new(square(-122.35, 37.75, 0.02), vec![]),
            ])),
            tags: tags(&[("natural", "water"), ("water", "lake")]),
        });
        out.push(Feature {
            id: OsmId::relation(3),
            kind: FeatureKind::Natural(NaturalKind::Wood),
            geometry: Geometry::Polygon(Polygon::new(square(-123.0, 37.0, 2.0), vec![])),
            tags: tags(&[("natural", "wood")]),
        });
        out.push(Feature {
            id: OsmId::relation(4),
            kind: FeatureKind::Boundary(BoundaryKind::Administrative),
            geometry: Geometry::Polygon(Polygon::new(square(-124.0, 36.0, 4.0), vec![])),
            tags: tags(&[
                ("boundary", "administrative"),
                ("admin_level", "6"),
                ("name", "County"),
            ]),
        });
        out.push(Feature {
            id: OsmId::way(99_999),
            kind: FeatureKind::Railway(RailwayKind::Rail),
            geometry: Geometry::Line(LineString::from(vec![
                (-123.5, 36.5),
                (-122.4, 37.8),
                (-121.0, 38.9),
            ])),
            tags: tags(&[("railway", "rail"), ("name", "Main Line")]),
        });
        out
    }

    /// FNV-1a: a hash that is stable across Rust releases.
    fn fnv1a(hash: &mut u64, bytes: &[u8]) {
        for &b in bytes {
            *hash ^= u64::from(b);
            *hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
        }
    }

    /// Hash of every tile (coordinate and bytes, in delivery order) and of
    /// the layer metadata, for `mode`.
    fn output_hashes(mode: AttributeMode) -> (u64, u64, u64) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(TagStore::new());
        let mut config = TileGeneratorConfig {
            temp_dir: Some(dir.path().to_path_buf()),
            // Uncompressed, so the hashes do not depend on the gzip backend.
            compression: TileCompression::None,
            ..Default::default()
        };
        config.render.attributes = mode;
        let g = TileGenerator::new(config, Box::new(MvtEncoder), Arc::clone(&store)).expect("new");
        g.add_parallel(&mixed_features(&store)).expect("add");
        let layers = g.stats().expect("stats").layers_by_name();
        let meta = metadata_json(&ArchiveInfo::default(), TileFormat::Mvt, &layers);
        let mut meta_hash = 0xCBF2_9CE4_8422_2325u64;
        fnv1a(&mut meta_hash, meta["vector_layers"].to_string().as_bytes());
        let mut tile_hash = 0xCBF2_9CE4_8422_2325u64;
        let summary = g
            .finish(|t| {
                fnv1a(&mut tile_hash, t.coord.to_string().as_bytes());
                fnv1a(&mut tile_hash, &t.data);
                Ok(())
            })
            .expect("finish");
        (tile_hash, meta_hash, summary.tiles)
    }

    /// Pins the generator's exact output, so performance work on the
    /// render → sort → assemble path is provably byte-for-byte neutral.
    /// Update the constants only for an intended output change.
    #[test]
    fn output_matches_golden_hashes() {
        let curated = output_hashes(AttributeMode::Curated);
        let all = output_hashes(AttributeMode::All);
        assert_eq!(
            curated,
            (13100065092465890892, 808955951512424232, 16234),
            "curated"
        );
        assert_eq!(
            all,
            (14680961121889460320, 13951613413496947922, 16234),
            "all tags"
        );
    }

    /// Run `f` on its own thread; fail instead of hanging if it deadlocks.
    fn within_a_minute<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(std::time::Duration::from_secs(60))
            .expect("finished (a timeout means a deadlock)")
    }

    fn pool(threads: usize) -> rayon::ThreadPool {
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .expect("pool")
    }

    #[test]
    fn in_order_delivery_from_inside_small_pools() {
        for threads in [1, 2] {
            for (parts, window) in [(0, 1), (1, 1), (7, 1), (7, 2), (7, 3), (9, 0), (4, 100)] {
                let got = within_a_minute(move || {
                    pool(threads).install(|| {
                        let mut got = Vec::new();
                        encode_in_order(
                            parts,
                            window,
                            |i| Ok::<_, ()>(vec![i * 10, i * 10 + 1]),
                            |v| {
                                got.push(v);
                                Ok(())
                            },
                        )
                        .map(|()| got)
                    })
                });
                let want: Vec<usize> = (0..parts).flat_map(|i| [i * 10, i * 10 + 1]).collect();
                assert_eq!(
                    got,
                    Ok(want),
                    "{threads} threads, {parts} parts, window {window}"
                );
            }
        }
    }

    #[test]
    fn in_order_delivery_stops_at_the_first_error() {
        for threads in [1, 2] {
            let (delivered, result) = within_a_minute(move || {
                pool(threads).install(|| {
                    let mut delivered = Vec::new();
                    let result = encode_in_order(
                        10,
                        2,
                        |i| if i == 5 { Err(i) } else { Ok(vec![i]) },
                        |v| {
                            delivered.push(v);
                            Ok(())
                        },
                    );
                    (delivered, result)
                })
            });
            assert_eq!(delivered, [0, 1, 2, 3, 4]);
            assert_eq!(result, Err(5));
            let result = within_a_minute(move || {
                pool(threads).install(|| {
                    encode_in_order(
                        10,
                        2,
                        |i| Ok(vec![i]),
                        |v| if v == 3 { Err(v) } else { Ok(()) },
                    )
                })
            });
            assert_eq!(result, Err(3));
        }
    }

    #[test]
    fn a_panicking_encoder_propagates_instead_of_hanging() {
        for threads in [1, 2] {
            let outcome = within_a_minute(move || {
                std::panic::catch_unwind(|| {
                    pool(threads).install(|| {
                        encode_in_order(
                            6,
                            2,
                            |i| -> Result<Vec<usize>, ()> {
                                assert!(i != 3, "encoder bug");
                                Ok(vec![i])
                            },
                            |_| Ok(()),
                        )
                    })
                })
            });
            assert!(outcome.is_err(), "{threads} threads");
        }
    }

    #[test]
    fn finish_inside_a_one_or_two_thread_pool_does_not_deadlock() {
        for threads in [1, 2] {
            let tiles = within_a_minute(move || {
                pool(threads).install(|| {
                    let dir = tempfile::tempdir().expect("tempdir");
                    let store = Arc::new(TagStore::new());
                    let config = TileGeneratorConfig {
                        temp_dir: Some(dir.path().to_path_buf()),
                        ..Default::default()
                    };
                    let g = TileGenerator::new(config, Box::new(MvtEncoder), Arc::clone(&store))
                        .expect("new");
                    g.add_parallel(&features(&store)).expect("add");
                    let mut tiles = 0u64;
                    g.finish(|_| {
                        tiles += 1;
                        Ok(())
                    })
                    .expect("finish");
                    tiles
                })
            });
            assert!(tiles > 0, "{threads} threads");
        }
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
