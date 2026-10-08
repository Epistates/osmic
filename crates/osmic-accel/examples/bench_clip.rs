//! Quick CPU-vs-GPU throughput comparison.
//!
//! ```text
//! cargo run --release -p osmic-accel --example bench_clip
//! ```

use std::io::Write;
use std::time::{Duration, Instant};

use geo_types::{Coord, LineString, Polygon};
use osmic_accel::{ClipOptions, GpuAccelerator, WorkItem, clip_batch_cpu};
use osmic_core::geometry::Geometry;
use osmic_core::mercator::tile_bounds;

/// xorshift64*: deterministic, dependency-free.
struct Rng(u64);

impl Rng {
    fn unit(&mut self) -> f64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        (self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64
    }

    fn range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.unit()
    }
}

const ZOOM: u8 = 8;
const TILE: (u32, u32) = (135, 90);

/// Center and half-size (degrees) of the benchmarked tile.
fn tile_frame() -> (f64, f64, f64, f64) {
    let b = tile_bounds(TILE.0, TILE.1, ZOOM);
    (
        (b.min_lon + b.max_lon) / 2.0,
        (b.min_lat + b.max_lat) / 2.0,
        b.width() / 2.0,
        b.height() / 2.0,
    )
}

fn polygon(rng: &mut Rng, vertices: usize) -> Geometry {
    // Around the tile; radii cross the tile edge.
    let (cx, cy, hw, hh) = tile_frame();
    let mut angles: Vec<f64> = (0..vertices)
        .map(|_| rng.range(0.0, std::f64::consts::TAU))
        .collect();
    angles.sort_by(f64::total_cmp);
    let mut ring: Vec<Coord<f64>> = angles
        .iter()
        .map(|a| {
            let r = rng.range(0.4, 1.6);
            Coord {
                x: cx + r * a.cos() * hw,
                y: cy + r * a.sin() * hh,
            }
        })
        .collect();
    ring.push(ring[0]);
    Geometry::Polygon(Polygon::new(LineString(ring), vec![]))
}

fn line(rng: &mut Rng, vertices: usize) -> Geometry {
    // A random walk from the tile center, crossing the tile edges.
    let (mut x, mut y, hw, hh) = tile_frame();
    Geometry::Line(LineString(
        (0..vertices)
            .map(|_| {
                x += rng.range(-0.4, 0.4) * hw;
                y += rng.range(-0.4, 0.4) * hh;
                Coord { x, y }
            })
            .collect(),
    ))
}

fn time<T>(runs: usize, mut f: impl FnMut() -> T) -> Duration {
    let _ = f(); // warm-up (pipeline state, page faults)
    let mut best = Duration::MAX;
    for _ in 0..runs {
        let t = Instant::now();
        let _ = f();
        best = best.min(t.elapsed());
    }
    best
}

fn main() {
    let mut out = std::io::stdout().lock();
    let gpu = GpuAccelerator::new().ok();
    if gpu.is_none() {
        let _ = writeln!(out, "GPU unavailable; CPU numbers only");
    }
    let opts = ClipOptions::default();

    let scenarios: [(&str, usize, usize, bool); 6] = [
        ("polygons  100 x   20 verts", 100, 20, true),
        ("polygons 10k  x   20 verts", 10_000, 20, true),
        ("polygons 10k  x  200 verts", 10_000, 200, true),
        ("polygons 200  x 5000 verts", 200, 5_000, true),
        ("lines    10k  x  100 verts", 10_000, 100, false),
        ("lines    200  x 5000 verts", 200, 5_000, false),
    ];
    for (name, count, vertices, is_polygon) in scenarios {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let geoms: Vec<Geometry> = (0..count)
            .map(|_| {
                if is_polygon {
                    polygon(&mut rng, vertices)
                } else {
                    line(&mut rng, vertices)
                }
            })
            .collect();
        let items: Vec<WorkItem<'_>> = geoms
            .iter()
            .map(|g| WorkItem {
                geometry: g,
                tile_x: TILE.0,
                tile_y: TILE.1,
                zoom: ZOOM,
                extent: 4096,
            })
            .collect();

        let cpu = time(25, || clip_batch_cpu(&items, &opts).map(|r| r.len()));
        let _ = write!(out, "{name}: cpu {:>9.3} ms", cpu.as_secs_f64() * 1e3);
        if let Some(gpu) = &gpu {
            let t = time(25, || gpu.clip_batch(&items).map(|r| r.len()));
            let _ = write!(
                out,
                "   gpu {:>9.3} ms   speedup {:>5.2}x",
                t.as_secs_f64() * 1e3,
                cpu.as_secs_f64() / t.as_secs_f64()
            );
        }
        let _ = writeln!(out);
    }
}
