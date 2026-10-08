//! GPU vs CPU parity. Every test skips (passes with a note) when Metal is not
//! available in this build or on this machine.

mod common;

use geo_types::MultiPolygon;
use osmic_accel::{clip_batch_cpu, AccelError, Backend, ClipOptions, Clipper, GpuAccelerator};
use osmic_core::geometry::Geometry;

use common::*;

fn gpu(options: ClipOptions) -> Option<GpuAccelerator> {
    match GpuAccelerator::with_options(options) {
        Ok(g) => Some(g),
        Err(AccelError::NotAvailable | AccelError::MetalInit(_)) => {
            eprintln!("skipping: Metal GPU backend unavailable");
            None
        }
        Err(e) => panic!("unexpected GPU init error: {e}"),
    }
}

fn compare(geoms: &[Geometry], options: ClipOptions, label: &str) {
    let Some(gpu) = gpu(options.clone()) else {
        return;
    };
    let items = items(geoms);
    let got = gpu.clip_batch(&items).unwrap();
    let want = clip_batch_cpu(&items, &ClipOptions::default()).unwrap();
    assert_eq!(got.len(), want.len());
    let mut dev = Deviation::default();
    for (i, (g, w)) in got.iter().zip(&want).enumerate() {
        assert_close(g, w, &mut dev, &format!("{label}[{i}]"));
    }
    eprintln!(
        "{label}: {} items, max deviation {} tile units",
        got.len(),
        dev.0
    );
}

#[test]
fn mixed_random_batch_matches_cpu() {
    let f = frame();
    let mut rng = Rng::new(0xC0FFEE);
    let geoms: Vec<Geometry> = (0..600).map(|_| random_geometry(&mut rng, &f)).collect();
    compare(&geoms, ClipOptions::default(), "mixed");
}

#[test]
fn concave_polygons_with_holes_match_cpu() {
    let f = frame();
    let mut rng = Rng::new(7);
    let geoms: Vec<Geometry> = (0..300)
        .map(|_| {
            let n = rng.int(8, 120);
            let holes = rng.int(1, 4);
            Geometry::Polygon(polygon_with_holes(&mut rng, &f, n, holes))
        })
        .collect();
    compare(&geoms, ClipOptions::default(), "polygons");
}

#[test]
fn multi_crossing_lines_match_cpu() {
    let f = frame();
    let mut rng = Rng::new(99);
    let geoms: Vec<Geometry> = (0..300)
        .map(|_| {
            let n = rng.int(20, 400);
            Geometry::Line(random_walk(&mut rng, &f, n, 1.2))
        })
        .collect();
    compare(&geoms, ClipOptions::default(), "lines");
}

#[test]
fn rings_beyond_the_old_2048_vertex_limit_match_cpu() {
    let f = frame();
    let mut rng = Rng::new(2025);
    let mut geoms = Vec::new();
    for n in [2047, 2048, 2049, 5000, 20_000] {
        geoms.push(Geometry::Polygon(polygon_with_holes(&mut rng, &f, n, 2)));
        geoms.push(Geometry::Line(random_walk(&mut rng, &f, n, 0.3)));
    }
    geoms.push(Geometry::MultiPolygon(MultiPolygon(
        (0..3)
            .map(|_| polygon_with_holes(&mut rng, &f, 3000, 1))
            .collect(),
    )));
    compare(&geoms, ClipOptions::default(), "large");
}

#[test]
fn capacity_overflow_falls_back_to_exact_cpu_results() {
    // Capacity equal to the input size makes any ring that needs new vertices
    // overflow on the GPU; the host must recompute those items on the CPU.
    let f = frame();
    let mut rng = Rng::new(31337);
    let mut geoms: Vec<Geometry> = (0..200)
        .map(|_| {
            let n = rng.int(10, 80);
            Geometry::Polygon(polygon_with_holes(&mut rng, &f, n, 2))
        })
        .collect();
    geoms.extend((0..20).map(|_| Geometry::Line(random_walk(&mut rng, &f, 100, 1.0))));
    let mut tight = ClipOptions::default();
    tight.ring_capacity_factor = 1;
    tight.ring_capacity_slack = 0;
    compare(&geoms, tight, "overflow-fallback");
}

#[test]
fn async_dispatch_returns_the_same_results() {
    let Some(gpu) = gpu(ClipOptions::default()) else {
        return;
    };
    let f = frame();
    let mut rng = Rng::new(5);
    let geoms: Vec<Geometry> = (0..100).map(|_| random_geometry(&mut rng, &f)).collect();
    let items = items(&geoms);
    let pending = gpu.clip_batch_async(&items).unwrap();
    // Work on the CPU while the GPU runs.
    let want = clip_batch_cpu(&items, &ClipOptions::default()).unwrap();
    let got = pending.wait().unwrap();
    let mut dev = Deviation::default();
    for (i, (g, w)) in got.iter().zip(&want).enumerate() {
        assert_close(g, w, &mut dev, &format!("async[{i}]"));
    }
}

#[test]
fn concurrent_use_from_many_threads() {
    let Some(gpu) = gpu(ClipOptions::default()) else {
        return;
    };
    let f = frame();
    std::thread::scope(|scope| {
        for t in 0..4u64 {
            let gpu = &gpu;
            let f = &f;
            scope.spawn(move || {
                let mut rng = Rng::new(100 + t);
                for _ in 0..10 {
                    let geoms: Vec<Geometry> =
                        (0..50).map(|_| random_geometry(&mut rng, f)).collect();
                    let items = items(&geoms);
                    let got = gpu.clip_batch(&items).unwrap();
                    let want = clip_batch_cpu(&items, &ClipOptions::default()).unwrap();
                    let mut dev = Deviation::default();
                    for (g, w) in got.iter().zip(&want) {
                        assert_close(g, w, &mut dev, "threaded");
                    }
                }
            });
        }
    });
}

#[test]
fn empty_and_point_only_batches_need_no_gpu_work() {
    let Some(gpu) = gpu(ClipOptions::default()) else {
        return;
    };
    assert!(gpu.clip_batch(&[]).unwrap().is_empty());
    let f = frame();
    let geoms = vec![Geometry::Point(geo_types::Point::new(f.cx, f.cy))];
    let got = gpu.clip_batch(&items(&geoms)).unwrap();
    assert_eq!(got.len(), 1);
    assert!(!got[0].is_empty());
}

#[test]
fn invalid_items_are_rejected_by_the_gpu_path_too() {
    let Some(gpu) = gpu(ClipOptions::default()) else {
        return;
    };
    let nan = Geometry::Point(geo_types::Point::new(f64::NAN, 0.0));
    let err = gpu.clip_batch(&items(&[nan])).unwrap_err();
    assert!(matches!(err, AccelError::InvalidInput(_)));
}

#[test]
fn clipper_selects_gpu_when_available() {
    let expected = if osmic_accel::is_available() {
        Backend::Gpu
    } else {
        Backend::Cpu
    };
    assert_eq!(Clipper::new().backend(), expected);
}
