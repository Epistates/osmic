//! Background tile loading: read, decode, evaluate the style and tessellate
//! on worker threads so the render loop never blocks.

use std::collections::{HashSet, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use osmic_core::TileCoord;
use osmic_render::{
    Mesh, RenderFeature, SceneBuilder, SceneOptions, TessellationOptions, tessellate_scene,
};
use osmic_style::Style;
use osmic_text::LabelCandidate;
use osmic_tiles::mvt_decode::{self, DecodedFeature};
use osmic_tiles::reader::{self, ArchiveReader};
use pmtiles::Compression;
use tracing::{debug, warn};

use crate::tile_cache::Weigh;

/// Layers whose named features can be clicked for details.
const POI_LAYERS: &[&str] = &[
    "amenity",
    "shop",
    "tourism",
    "office",
    "healthcare",
    "craft",
    "historic",
    "leisure",
    "club",
    "emergency",
    "education",
];

/// A clickable named feature.
#[derive(Debug, Clone, PartialEq)]
pub struct Poi {
    pub lon: f64,
    pub lat: f64,
    pub layer: String,
    pub class: Option<String>,
    pub name: String,
    pub tags: Vec<(String, String)>,
}

/// Everything the viewer needs from one tile, produced off the render
/// thread.
pub struct TileData {
    /// Tessellated geometry in tile-local pixels.
    pub mesh: Mesh,
    /// Labels awaiting placement, anchored in tile-local pixels.
    pub labels: Vec<LabelCandidate>,
    pub pois: Vec<Poi>,
}

impl TileData {
    /// Evaluate `style` over `features` at the tile's zoom and tessellate.
    ///
    /// The style is evaluated at the tile's integer zoom; widths are also
    /// evaluated one zoom up so the vertex shader can interpolate between
    /// them as the camera zooms.
    pub fn build(coord: TileCoord, features: &[DecodedFeature], style: &Style) -> Self {
        let scene = SceneBuilder::new(style).build(features, &SceneOptions::for_tile(coord));
        let mesh = tessellate_scene(&scene, &TessellationOptions::default());
        if mesh.truncated || mesh.skipped > 0 {
            debug!(%coord, truncated = mesh.truncated, skipped = mesh.skipped, "tile mesh incomplete");
        }
        let labels = scene
            .layers
            .into_iter()
            .flat_map(|l| l.features)
            .filter_map(|f| match f {
                RenderFeature::Label(l) => Some(l),
                _ => None,
            })
            .collect();
        let pois = features
            .iter()
            .filter(|f| POI_LAYERS.contains(&f.layer.as_str()))
            .filter_map(|f| {
                let name = f.name.clone().filter(|n| !n.is_empty())?;
                let center = f.geometry.bbox().center();
                Some(Poi {
                    lon: center.lon,
                    lat: center.lat,
                    layer: f.layer.clone(),
                    class: f.class.clone(),
                    name,
                    tags: f
                        .tags
                        .iter()
                        .map(|(k, v)| (k.clone(), v.to_string()))
                        .collect(),
                })
            })
            .collect();
        Self { mesh, labels, pois }
    }
}

impl Weigh for TileData {
    fn weight(&self) -> usize {
        self.mesh.vertices.len() * std::mem::size_of::<osmic_render::MeshVertex>()
            + self.mesh.indices.len() * 4
            + self.labels.len() * 160
            + self.pois.len() * 200
    }
}

/// Where raw tile bytes come from.
pub trait TileSource: Send + Sync {
    /// The decompressed MVT bytes of `coord`, or `None` if the source has
    /// no such tile.
    fn fetch(&self, coord: TileCoord) -> Result<Option<Vec<u8>>, String>;
}

/// A PMTiles archive opened for reading.
pub struct PmtilesSource {
    reader: ArchiveReader,
    runtime: tokio::runtime::Runtime,
    compression: Compression,
    /// Largest decompressed tile accepted.
    max_tile_bytes: u64,
    /// Highest zoom level stored in the archive.
    pub max_zoom: u8,
    /// Center suggested by the archive header: `(lon, lat, zoom)`.
    pub center: Option<(f64, f64, f64)>,
}

impl PmtilesSource {
    /// Open `path`. A missing, unreadable or non-PMTiles file, a malformed
    /// header, or tile compression the viewer cannot read is an error.
    pub fn open(path: &Path) -> Result<Self, String> {
        // One worker thread so concurrent `block_on` calls from the tile
        // workers are all driven.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .map_err(|e| format!("starting async runtime: {e}"))?;
        let reader = runtime
            .block_on(reader::open(path, reader::DEFAULT_DIRECTORY_CACHE))
            .map_err(|e| describe(&e))?;
        let header = reader.get_header();
        // What `reader::decompress_tile` can inflate; failing here beats
        // failing every tile.
        let compression = header.tile_compression;
        if !matches!(compression, Compression::None | Compression::Gzip) {
            return Err(format!(
                "{} uses {compression:?} tile compression, which the viewer cannot read",
                path.display()
            ));
        }
        let max_zoom = header.max_zoom;
        let has_center = header.center_longitude.is_finite()
            && header.center_latitude.is_finite()
            && (header.center_longitude != 0.0 || header.center_latitude != 0.0);
        let center = has_center.then_some((
            header.center_longitude,
            header.center_latitude,
            f64::from(header.center_zoom),
        ));
        Ok(Self {
            reader,
            runtime,
            compression,
            max_tile_bytes: reader::MAX_DECOMPRESSED_TILE,
            max_zoom,
            center,
        })
    }
}

impl TileSource for PmtilesSource {
    fn fetch(&self, coord: TileCoord) -> Result<Option<Vec<u8>>, String> {
        let pm = pmtiles::TileCoord::new(coord.z.0, coord.x, coord.y)
            .map_err(|e| format!("tile {coord}: {e}"))?;
        let Some(raw) = self
            .runtime
            .block_on(self.reader.get_tile(pm))
            .map_err(|e| format!("reading tile {coord}: {e}"))?
        else {
            return Ok(None);
        };
        reader::decompress_tile(&raw, self.compression, self.max_tile_bytes)
            .map(|tile| Some(tile.into_owned()))
            .map_err(|e| format!("tile {coord}: {}", describe(&e)))
    }
}

/// `error` followed by its sources, separated by `: `.
fn describe(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(e) = source {
        text.push_str(": ");
        text.push_str(&e.to_string());
        source = e.source();
    }
    text
}

/// The outcome of loading one tile.
pub struct LoadedTile {
    pub coord: TileCoord,
    /// `Ok(None)`: the source has no such tile.
    pub result: Result<Option<TileData>, String>,
}

#[derive(Default)]
struct Queue {
    pending: VecDeque<TileCoord>,
    /// Tiles being loaded, or loaded but not yet taken by
    /// [`TileLoader::try_recv`]. Kept until the receiver has the result, so
    /// a request between the worker sending it and the receiver draining
    /// it cannot queue the tile again.
    in_flight: HashSet<TileCoord>,
    shutdown: bool,
}

struct Shared {
    queue: Mutex<Queue>,
    wake: Condvar,
}

/// Loads tiles on background threads.
///
/// [`TileLoader::request`] replaces the list of wanted tiles, so tiles the
/// user has already scrolled away from are dropped before they start.
pub struct TileLoader {
    shared: Arc<Shared>,
    results: Receiver<LoadedTile>,
    workers: Vec<JoinHandle<()>>,
}

impl TileLoader {
    /// Start `workers` threads loading from `source`. `notify` is called
    /// (from a worker thread) whenever a result becomes available — use it
    /// to wake the event loop.
    pub fn spawn(
        source: Arc<dyn TileSource>,
        style: Arc<Style>,
        workers: usize,
        notify: impl Fn() + Send + Sync + 'static,
    ) -> std::io::Result<Self> {
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue::default()),
            wake: Condvar::new(),
        });
        let (tx, results) = channel();
        let notify = Arc::new(notify);
        let workers = (0..workers.max(1))
            .map(|i| {
                let (shared, source, style, tx, notify) = (
                    Arc::clone(&shared),
                    Arc::clone(&source),
                    Arc::clone(&style),
                    tx.clone(),
                    Arc::clone(&notify),
                );
                std::thread::Builder::new()
                    .name(format!("tile-loader-{i}"))
                    .spawn(move || worker(&shared, source.as_ref(), &style, &tx, notify.as_ref()))
            })
            .collect::<std::io::Result<Vec<_>>>()?;
        Ok(Self {
            shared,
            results,
            workers,
        })
    }

    /// Set the tiles wanted, most important first. Tiles already being
    /// loaded are left alone; previously requested tiles that have not
    /// started are forgotten.
    pub fn request(&self, wanted: &[TileCoord]) {
        let mut q = self.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
        q.pending.clear();
        let mut seen = HashSet::new();
        for c in wanted {
            if !q.in_flight.contains(c) && seen.insert(*c) {
                q.pending.push_back(*c);
            }
        }
        drop(q);
        self.shared.wake.notify_all();
    }

    /// A finished tile, if any. From here on the caller owns it: it is no
    /// longer in flight, so a later [`TileLoader::request`] may load it
    /// again.
    pub fn try_recv(&self) -> Option<LoadedTile> {
        let tile = self.results.try_recv().ok()?;
        self.shared
            .queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .in_flight
            .remove(&tile.coord);
        Some(tile)
    }
}

impl Drop for TileLoader {
    fn drop(&mut self) {
        {
            let mut q = self.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
            q.shutdown = true;
            q.pending.clear();
        }
        self.shared.wake.notify_all();
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
    }
}

fn worker(
    shared: &Shared,
    source: &dyn TileSource,
    style: &Style,
    tx: &Sender<LoadedTile>,
    notify: &(dyn Fn() + Send + Sync),
) {
    loop {
        let coord = {
            let mut q = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if q.shutdown {
                    return;
                }
                if let Some(c) = q.pending.pop_front() {
                    q.in_flight.insert(c);
                    break c;
                }
                q = shared.wake.wait(q).unwrap_or_else(|e| e.into_inner());
            }
        };
        // A bug in decoding or tessellation must not take the worker (and
        // with it, loading) down: report the tile as failed instead.
        let result = catch_unwind(AssertUnwindSafe(|| load(source, style, coord)))
            .unwrap_or_else(|_| Err(format!("internal error while loading tile {coord}")));
        if let Err(e) = &result {
            warn!(%coord, error = %e, "tile failed to load");
        }
        // The tile stays in flight until `try_recv` takes it.
        let _ = tx.send(LoadedTile { coord, result });
        notify();
    }
}

fn load(
    source: &dyn TileSource,
    style: &Style,
    coord: TileCoord,
) -> Result<Option<TileData>, String> {
    let Some(bytes) = source.fetch(coord)? else {
        return Ok(None);
    };
    let features = mvt_decode::decode_tile(&bytes, coord)
        .map_err(|e| format!("decoding tile {coord}: {e}"))?;
    Ok(Some(TileData::build(coord, &features, style)))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use geo_types::{LineString, Point, Polygon};
    use osmic_core::{Geometry, Zoom};

    use super::*;

    fn t(x: u32) -> TileCoord {
        TileCoord::new(x, 0, Zoom(6))
    }

    fn wait_for(loader: &TileLoader, n: usize) -> Vec<LoadedTile> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut out = Vec::new();
        while out.len() < n {
            assert!(
                Instant::now() < deadline,
                "timed out; got {} of {n}",
                out.len()
            );
            match loader.try_recv() {
                Some(t) => out.push(t),
                None => std::thread::sleep(Duration::from_millis(2)),
            }
        }
        out
    }

    /// Fake source: serves empty tiles, records fetch order, can block.
    struct FakeSource {
        fetched: Mutex<Vec<TileCoord>>,
        gate: Mutex<bool>,
        gate_cv: Condvar,
        fail_on: Option<u32>,
    }

    impl FakeSource {
        fn new(open: bool) -> Arc<Self> {
            Arc::new(Self {
                fetched: Mutex::new(Vec::new()),
                gate: Mutex::new(open),
                gate_cv: Condvar::new(),
                fail_on: None,
            })
        }

        fn open_gate(&self) {
            *self.gate.lock().unwrap() = true;
            self.gate_cv.notify_all();
        }
    }

    impl TileSource for FakeSource {
        fn fetch(&self, coord: TileCoord) -> Result<Option<Vec<u8>>, String> {
            self.fetched.lock().unwrap().push(coord);
            let mut open = self.gate.lock().unwrap();
            while !*open {
                open = self.gate_cv.wait(open).unwrap();
            }
            if self.fail_on == Some(coord.x) {
                return Err("boom".into());
            }
            if coord.x == 99 {
                return Ok(None);
            }
            Ok(Some(Vec::new()))
        }
    }

    fn style() -> Arc<Style> {
        Arc::new(osmic_style::default_style())
    }

    #[test]
    fn loads_requested_tiles_and_notifies() {
        let notified = Arc::new(AtomicUsize::new(0));
        let n = Arc::clone(&notified);
        let loader = TileLoader::spawn(FakeSource::new(true), style(), 2, move || {
            n.fetch_add(1, Ordering::SeqCst);
        })
        .unwrap();
        loader.request(&[t(1), t(2), t(99)]);
        let mut got = wait_for(&loader, 3);
        got.sort_by_key(|g| g.coord.x);
        assert!(got[0].result.as_ref().unwrap().is_some());
        assert!(got[1].result.as_ref().unwrap().is_some());
        assert!(got[2].result.as_ref().unwrap().is_none(), "missing tile");
        let deadline = Instant::now() + Duration::from_secs(5);
        while notified.load(Ordering::SeqCst) < 3 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(notified.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn a_new_request_replaces_tiles_that_have_not_started() {
        let source = FakeSource::new(false);
        let loader = TileLoader::spawn(source.clone(), style(), 1, || {}).unwrap();
        loader.request(&[t(1)]);
        // Wait until the single worker is blocked inside fetch(1).
        let deadline = Instant::now() + Duration::from_secs(5);
        while source.fetched.lock().unwrap().is_empty() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
        loader.request(&[t(2), t(3)]);
        loader.request(&[t(4), t(1), t(5), t(4)]); // 1 is in flight; 4 repeated
        source.open_gate();
        let got = wait_for(&loader, 3);
        let mut xs: Vec<u32> = got.iter().map(|g| g.coord.x).collect();
        assert_eq!(xs.remove(0), 1, "the in-flight tile completes first");
        assert_eq!(
            xs,
            vec![4, 5],
            "priority order kept, stale 2 and 3 forgotten, duplicates and in-flight skipped"
        );
        assert_eq!(
            source
                .fetched
                .lock()
                .unwrap()
                .iter()
                .map(|c| c.x)
                .collect::<Vec<_>>(),
            vec![1, 4, 5]
        );
    }

    #[test]
    fn a_finished_tile_not_yet_received_is_not_loaded_again() {
        // The render loop drains results, then requests what its cache
        // lacks. A tile finishing between the two must not be queued
        // again just because the drain came too early.
        let source = FakeSource::new(true);
        let notified = Arc::new(AtomicUsize::new(0));
        let n = Arc::clone(&notified);
        let loader = TileLoader::spawn(source.clone(), style(), 1, move || {
            n.fetch_add(1, Ordering::SeqCst);
        })
        .unwrap();
        loader.request(&[t(1)]);
        let deadline = Instant::now() + Duration::from_secs(5);
        while notified.load(Ordering::SeqCst) == 0 {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
        // Sent and announced, not yet received: still in flight.
        loader.request(&[t(1)]);
        {
            let q = loader.shared.queue.lock().unwrap();
            assert!(q.pending.is_empty() && q.in_flight.contains(&t(1)));
        }
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(wait_for(&loader, 1)[0].coord, t(1));
        assert!(loader.try_recv().is_none(), "loaded once");
        assert_eq!(source.fetched.lock().unwrap().len(), 1);
        assert!(
            loader.shared.queue.lock().unwrap().in_flight.is_empty(),
            "received tiles are no longer in flight"
        );
    }

    #[test]
    fn failures_are_reported_per_tile_and_do_not_stop_the_worker() {
        let mut source = FakeSource::new(true);
        Arc::get_mut(&mut source).unwrap().fail_on = Some(2);
        let loader = TileLoader::spawn(source, style(), 1, || {}).unwrap();
        loader.request(&[t(2), t(3)]);
        let got = wait_for(&loader, 2);
        assert_eq!(
            got[0].result.as_ref().err().map(String::as_str),
            Some("boom")
        );
        assert!(got[1].result.as_ref().unwrap().is_some());
    }

    struct Corrupt;

    impl TileSource for Corrupt {
        fn fetch(&self, _: TileCoord) -> Result<Option<Vec<u8>>, String> {
            Ok(Some(vec![
                0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            ]))
        }
    }

    #[test]
    fn corrupt_tiles_are_errors_not_panics() {
        let loader = TileLoader::spawn(Arc::new(Corrupt), style(), 1, || {}).unwrap();
        loader.request(&[t(1)]);
        let got = wait_for(&loader, 1);
        assert!(got[0].result.is_err());
    }

    #[test]
    fn dropping_the_loader_stops_the_workers() {
        let source = FakeSource::new(true);
        let loader = TileLoader::spawn(source, style(), 3, || {}).unwrap();
        loader.request(&[t(1), t(2)]);
        drop(loader); // must join without hanging
    }

    #[test]
    fn tile_data_contains_geometry_labels_and_pois() {
        let coord = TileCoord::new(2618, 6332, Zoom(15));
        let bb = coord.bbox();
        let at = |fx: f64, fy: f64| (bb.min_lon + bb.width() * fx, bb.min_lat + bb.height() * fy);
        let feature =
            |layer: &str, class: &str, name: Option<&str>, geometry: Geometry| DecodedFeature {
                layer: layer.into(),
                id: None,
                class: Some(class.into()),
                name: name.map(str::to_string),
                tags: vec![("phone".into(), "123".into())],
                geometry,
            };
        let features = vec![
            feature(
                "landuse",
                "forest",
                None,
                Geometry::Polygon(Polygon::new(
                    LineString::from(vec![
                        at(0.1, 0.1),
                        at(0.9, 0.1),
                        at(0.9, 0.9),
                        at(0.1, 0.9),
                        at(0.1, 0.1),
                    ]),
                    vec![],
                )),
            ),
            feature(
                "highway",
                "primary",
                Some("Main St"),
                Geometry::Line(LineString::from(vec![at(0.0, 0.5), at(1.0, 0.5)])),
            ),
            feature(
                "shop",
                "bakery",
                Some("Bread"),
                Geometry::Point(Point::new(at(0.5, 0.5).0, at(0.5, 0.5).1)),
            ),
            feature(
                "shop",
                "kiosk",
                None,
                Geometry::Point(Point::new(at(0.2, 0.2).0, at(0.2, 0.2).1)),
            ),
        ];
        let data = TileData::build(coord, &features, &style());
        assert!(!data.mesh.vertices.is_empty() && data.mesh.indices.len().is_multiple_of(3));
        assert!(
            data.mesh
                .indices
                .iter()
                .all(|&i| (i as usize) < data.mesh.vertices.len())
        );
        let texts: Vec<&str> = data.labels.iter().map(|l| l.text.as_str()).collect();
        assert!(
            texts.contains(&"Main St") && texts.contains(&"Bread"),
            "{texts:?}"
        );
        assert_eq!(
            data.pois.len(),
            1,
            "only named points of interest are clickable"
        );
        assert_eq!(data.pois[0].name, "Bread");
        assert!((data.pois[0].lon - (bb.min_lon + bb.max_lon) / 2.0).abs() < 1e-9);
        assert!(data.weight() > 0);
    }

    /// Write a one-tile PMTiles archive (z8/70/95) with a forest polygon, a
    /// named road and a named shop, uncompressed.
    fn write_archive(path: &Path) {
        write_archive_with(path, osmic_tiles::assemble::TileCompression::None, |t| t);
    }

    /// Like [`write_archive`], storing `pack(tile)` with `compression`.
    fn write_archive_with(
        path: &Path,
        compression: osmic_tiles::assemble::TileCompression,
        pack: impl FnOnce(Vec<u8>) -> Vec<u8>,
    ) {
        use osmic_core::BBox;
        use osmic_tiles::encode::TileFormat;
        use osmic_tiles::model::{GeomType, TileFeature, TileLayer};
        use osmic_tiles::mvt::encode_tile;
        use osmic_tiles::pmtiles::{ArchiveOptions, PmTilesArchive};

        let feature = |geom_type, parts: Vec<Vec<[i32; 2]>>, attrs: &[(&str, &str)]| TileFeature {
            id: None,
            geom_type,
            parts,
            attributes: attrs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        };
        let layer = |name: &str, features| TileLayer {
            name: name.into(),
            extent: 4096,
            features,
        };
        let tile = encode_tile(&[
            layer(
                "landuse",
                vec![feature(
                    GeomType::Polygon,
                    vec![vec![[500, 500], [3500, 500], [3500, 3500], [500, 3500]]],
                    &[("class", "forest")],
                )],
            ),
            layer(
                "highway",
                vec![feature(
                    GeomType::LineString,
                    vec![vec![[0, 2000], [4096, 2000]]],
                    &[("class", "primary"), ("name", "Main St")],
                )],
            ),
            layer(
                "shop",
                vec![feature(
                    GeomType::Point,
                    vec![vec![[2048, 1000]]],
                    &[("class", "bakery"), ("name", "Bread")],
                )],
            ),
        ]);
        let mut archive = PmTilesArchive::create(
            path,
            &ArchiveOptions {
                format: TileFormat::Mvt,
                compression,
                bounds: BBox::new(-90.0, 0.0, 0.0, 66.0),
                min_zoom: 8,
                max_zoom: 8,
                metadata: serde_json::json!({}),
                overwrite: true,
            },
        )
        .unwrap();
        archive
            .add_tile(TileCoord::new(70, 95, Zoom(8)), &pack(tile))
            .unwrap();
        archive.finalize().unwrap();
    }

    fn gzip(data: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(data).unwrap();
        enc.finish().unwrap()
    }

    #[test]
    fn gzip_tiles_are_inflated_within_the_cap() {
        use osmic_tiles::assemble::TileCompression;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gz.pmtiles");
        write_archive_with(&path, TileCompression::Gzip, |t| gzip(&t));
        let present = TileCoord::new(70, 95, Zoom(8));

        let mut source = PmtilesSource::open(&path).unwrap();
        let tile = source.fetch(present).unwrap().expect("present");
        assert!(mvt_decode::decode_tile(&tile, present).is_ok());

        // The same tile is refused once it inflates past the cap.
        source.max_tile_bytes = tile.len() as u64 - 1;
        let err = source.fetch(present).unwrap_err();
        assert!(err.contains("more than"), "{err}");
    }

    #[test]
    fn a_decompression_bomb_is_an_error_not_an_allocation() {
        use osmic_tiles::assemble::TileCompression;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bomb.pmtiles");
        // 65 MiB of zeros: past the default 64 MiB cap, ~65 KiB packed.
        write_archive_with(&path, TileCompression::Gzip, |_| gzip(&vec![0; 65 << 20]));
        let source = PmtilesSource::open(&path).unwrap();
        let err = source.fetch(TileCoord::new(70, 95, Zoom(8))).unwrap_err();
        assert!(err.contains("more than"), "{err}");
    }

    #[test]
    fn reads_a_real_pmtiles_archive_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tiny.pmtiles");
        write_archive(&path);

        let source = Arc::new(PmtilesSource::open(&path).unwrap());
        assert_eq!(source.max_zoom, 8);
        let (lon, lat, _) = source.center.expect("the archive records a center");
        assert!((lon + 45.0).abs() < 1e-6 && (lat - 33.0).abs() < 1e-6);
        assert!(
            source
                .fetch(TileCoord::new(0, 0, Zoom(8)))
                .unwrap()
                .is_none(),
            "no such tile"
        );

        let loader = TileLoader::spawn(source, style(), 2, || {}).unwrap();
        let present = TileCoord::new(70, 95, Zoom(8));
        let absent = TileCoord::new(3, 3, Zoom(8));
        loader.request(&[present, absent]);
        let got = wait_for(&loader, 2);
        let tile = got
            .iter()
            .find(|g| g.coord == present)
            .unwrap()
            .result
            .as_ref()
            .unwrap()
            .as_ref()
            .unwrap();
        assert!(
            tile.mesh.vertices.len() > 4,
            "forest fill and road stroke are tessellated"
        );
        assert!(
            got.iter()
                .find(|g| g.coord == absent)
                .unwrap()
                .result
                .as_ref()
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn bad_archives_are_errors_not_panics() {
        let dir = tempfile::tempdir().unwrap();
        let missing = PmtilesSource::open(&dir.path().join("nope.pmtiles"))
            .err()
            .expect("missing file");
        assert!(missing.contains("nope.pmtiles"), "{missing}");
        let corrupt = dir.path().join("corrupt.pmtiles");
        std::fs::write(&corrupt, b"this is not a pmtiles archive at all, just text").unwrap();
        let err = PmtilesSource::open(&corrupt).err().expect("corrupt file");
        assert!(err.contains("corrupt.pmtiles"), "{err}");
        let empty = dir.path().join("empty.pmtiles");
        std::fs::write(&empty, b"").unwrap();
        assert!(PmtilesSource::open(&empty).is_err());

        // A valid magic and version but a root directory past the end of
        // the file: the `pmtiles` reader would slice out of bounds.
        let mut header = b"PMTiles\x03".to_vec();
        for field in [127u64, 100_000, 0, 0, 0, 0, 0, 0] {
            header.extend_from_slice(&field.to_le_bytes());
        }
        header.resize(200, 0);
        let malformed = dir.path().join("malformed.pmtiles");
        std::fs::write(&malformed, &header).unwrap();
        let err = PmtilesSource::open(&malformed)
            .err()
            .expect("malformed header");
        assert!(
            err.contains("malformed.pmtiles") && err.contains("root directory"),
            "{err}"
        );
    }
}
