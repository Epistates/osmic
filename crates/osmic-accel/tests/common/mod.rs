//! Shared helpers for the integration tests: deterministic PRNG, geometry
//! generators positioned around a tile, and tolerance-based comparison.
#![allow(dead_code)]

use geo_types::{Coord, LineString, MultiPolygon, Point, Polygon};
use osmic_accel::{
    AccelError, AccelResult, ClipOptions, ClippedGeometry, GpuAccelerator, WorkItem, clip_batch_cpu,
};
use osmic_core::geometry::Geometry;

/// The GPU backend, or `None` (the test skips) when this build or machine
/// has no Metal. Any other initialisation failure fails the test.
pub fn gpu(options: ClipOptions) -> Option<GpuAccelerator> {
    match GpuAccelerator::with_options(options) {
        Ok(g) => Some(g),
        Err(AccelError::NotAvailable) => {
            eprintln!("skipping: Metal GPU backend unavailable");
            None
        }
        Err(e) => panic!("unexpected GPU init error: {e}"),
    }
}

/// Clip `items` with default options on every backend available here: the
/// CPU, and the GPU when there is one. Returns `(backend, result)` pairs.
pub fn on_every_backend(
    items: &[WorkItem<'_>],
) -> Vec<(&'static str, AccelResult<Vec<ClippedGeometry>>)> {
    let mut out = vec![("cpu", clip_batch_cpu(items, &ClipOptions::default()))];
    if let Some(gpu) = gpu(ClipOptions::default()) {
        out.push(("gpu", gpu.clip_batch(items)));
    }
    out
}

pub const ZOOM: u8 = 8;
pub const TILE_X: u32 = 135;
pub const TILE_Y: u32 = 90;
pub const EXTENT: u32 = 4096;

/// xorshift64*: deterministic, dependency-free.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }

    fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in [0, 1).
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    pub fn range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.unit()
    }

    pub fn int(&mut self, lo: usize, hi: usize) -> usize {
        lo + (self.next_u64() % (hi - lo + 1) as u64) as usize
    }
}

/// Lon/lat of the tile's north-west corner.
fn tile_corner(x: u32, y: u32, z: u8) -> (f64, f64) {
    let n = f64::from(1u32 << z);
    let lon = f64::from(x) / n * 360.0 - 180.0;
    let lat = (std::f64::consts::PI * (1.0 - 2.0 * f64::from(y) / n))
        .sinh()
        .atan()
        .to_degrees();
    (lon, lat)
}

/// Tile center and half-size (lon, lat degrees).
pub struct TileFrame {
    pub cx: f64,
    pub cy: f64,
    pub hw: f64,
    pub hh: f64,
}

pub fn frame() -> TileFrame {
    let (w, n) = tile_corner(TILE_X, TILE_Y, ZOOM);
    let (e, s) = tile_corner(TILE_X + 1, TILE_Y + 1, ZOOM);
    TileFrame {
        cx: (w + e) / 2.0,
        cy: (n + s) / 2.0,
        hw: (e - w) / 2.0,
        hh: (n - s) / 2.0,
    }
}

/// Star-shaped (generally concave) ring around `(cx, cy)`; radii are in units
/// of the tile half-size, so values above ~1 cross the tile boundary.
pub fn star(
    rng: &mut Rng,
    f: &TileFrame,
    cx: f64,
    cy: f64,
    vertices: usize,
    r_lo: f64,
    r_hi: f64,
) -> LineString<f64> {
    let mut angles: Vec<f64> = (0..vertices)
        .map(|_| rng.range(0.0, std::f64::consts::TAU))
        .collect();
    angles.sort_by(f64::total_cmp);
    let mut coords: Vec<Coord<f64>> = angles
        .iter()
        .map(|a| {
            let r = rng.range(r_lo, r_hi);
            Coord {
                x: cx + r * a.cos() * f.hw,
                y: cy + r * a.sin() * f.hh,
            }
        })
        .collect();
    coords.push(coords[0]);
    LineString(coords)
}

pub fn polygon_with_holes(
    rng: &mut Rng,
    f: &TileFrame,
    vertices: usize,
    holes: usize,
) -> Polygon<f64> {
    let cx = f.cx + rng.range(-0.8, 0.8) * f.hw;
    let cy = f.cy + rng.range(-0.8, 0.8) * f.hh;
    let exterior = star(rng, f, cx, cy, vertices, 0.6, 1.8);
    let interiors = (0..holes)
        .map(|_| {
            let hx = cx + rng.range(-0.2, 0.2) * f.hw;
            let hy = cy + rng.range(-0.2, 0.2) * f.hh;
            let n = rng.int(4, vertices.clamp(4, 30));
            star(rng, f, hx, hy, n, 0.05, 0.25)
        })
        .collect();
    Polygon::new(exterior, interiors)
}

pub fn random_walk(rng: &mut Rng, f: &TileFrame, vertices: usize, step: f64) -> LineString<f64> {
    let mut x = f.cx + rng.range(-1.5, 1.5) * f.hw;
    let mut y = f.cy + rng.range(-1.5, 1.5) * f.hh;
    let mut coords = Vec::with_capacity(vertices);
    for _ in 0..vertices {
        coords.push(Coord { x, y });
        x += rng.range(-step, step) * f.hw;
        y += rng.range(-step, step) * f.hh;
    }
    LineString(coords)
}

pub fn random_geometry(rng: &mut Rng, f: &TileFrame) -> Geometry {
    match rng.int(0, 5) {
        0 => Geometry::Point(Point::new(
            f.cx + rng.range(-1.3, 1.3) * f.hw,
            f.cy + rng.range(-1.3, 1.3) * f.hh,
        )),
        1 | 2 => {
            let n = rng.int(30, 200);
            Geometry::Line(random_walk(rng, f, n, 0.9))
        }
        3 => {
            let n = rng.int(5, 60);
            let holes = rng.int(0, 3);
            Geometry::Polygon(polygon_with_holes(rng, f, n, holes))
        }
        _ => {
            let parts = rng.int(1, 4);
            let polys = (0..parts)
                .map(|_| {
                    let n = rng.int(5, 40);
                    let holes = rng.int(0, 2);
                    polygon_with_holes(rng, f, n, holes)
                })
                .collect();
            Geometry::MultiPolygon(MultiPolygon(polys))
        }
    }
}

pub fn items(geoms: &[Geometry]) -> Vec<WorkItem<'_>> {
    geoms
        .iter()
        .map(|g| WorkItem {
            geometry: g,
            tile_x: TILE_X,
            tile_y: TILE_Y,
            zoom: ZOOM,
            extent: EXTENT,
        })
        .collect()
}

/// Absolute tolerance in tile units (extent 4096; one f32 ulp at 4096 is
/// about 5e-4).
pub const TOL: f32 = 0.005;

/// Largest coordinate deviation seen by `assert_close`.
#[derive(Default)]
pub struct Deviation(pub f32);

fn close(a: &[f32; 2], b: &[f32; 2], dev: &mut Deviation, what: &str) {
    let d = (a[0] - b[0]).abs().max((a[1] - b[1]).abs());
    dev.0 = dev.0.max(d);
    assert!(d <= TOL, "{what}: {a:?} vs {b:?} differ by {d}");
}

fn close_path(a: &[[f32; 2]], b: &[[f32; 2]], dev: &mut Deviation, what: &str) {
    assert_eq!(a.len(), b.len(), "{what}: vertex count");
    for (p, q) in a.iter().zip(b) {
        close(p, q, dev, what);
    }
}

pub fn assert_close(a: &ClippedGeometry, b: &ClippedGeometry, dev: &mut Deviation, what: &str) {
    match (a, b) {
        (ClippedGeometry::Empty, ClippedGeometry::Empty) => {}
        (ClippedGeometry::Point(p), ClippedGeometry::Point(q)) => close(p, q, dev, what),
        (ClippedGeometry::Lines(x), ClippedGeometry::Lines(y)) => {
            assert_eq!(x.len(), y.len(), "{what}: part count");
            for (p, q) in x.iter().zip(y) {
                close_path(p, q, dev, what);
            }
        }
        (ClippedGeometry::Polygons(x), ClippedGeometry::Polygons(y)) => {
            assert_eq!(x.len(), y.len(), "{what}: polygon count");
            for (p, q) in x.iter().zip(y) {
                close_path(&p.exterior, &q.exterior, dev, what);
                assert_eq!(p.holes.len(), q.holes.len(), "{what}: hole count");
                for (h, k) in p.holes.iter().zip(&q.holes) {
                    close_path(h, k, dev, what);
                }
            }
        }
        _ => panic!("{what}: result kinds differ: {a:?} vs {b:?}"),
    }
}
