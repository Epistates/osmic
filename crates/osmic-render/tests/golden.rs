//! Golden-image tests for the software backend.
//!
//! Each test renders a small fixed scene with the bundled OFL test font and
//! compares it with a checked-in PNG under `tests/golden/`. Differences of a
//! few levels per channel are tolerated (anti-aliasing arithmetic may differ
//! between tiny-skia/swash releases); anything else fails and the actual
//! image is written to `target/tmp/golden-failures/` for inspection.
//!
//! To (re)generate the goldens after an intentional change:
//!
//! ```sh
//! OSMIC_UPDATE_GOLDEN=1 cargo test -p osmic-render --test golden
//! ```

use std::path::{Path, PathBuf};

use geo_types::{LineString, Point, Polygon};
use osmic_core::{Color, Geometry};
use osmic_render::scene::{LineCap, LineJoin};
use osmic_render::{
    Camera, RenderBackend, RenderConfig, RenderFeature, RenderLayer, SceneBuilder, SceneGraph,
    SceneOptions, SkiaBackend,
};
use osmic_style::default_style;
use osmic_text::{LabelAnchor, LabelCandidate, LabelStyle, TextEngine};
use osmic_tiles::mvt_decode::DecodedFeature;
use tiny_skia::Pixmap;

/// Largest allowed difference of any channel of any pixel.
const CHANNEL_TOLERANCE: i16 = 3;

fn backend(width: u32, height: u32, ratio: f32) -> SkiaBackend {
    let fonts = [include_bytes!("fonts/Cantarell-Regular.ttf").to_vec()];
    let config = RenderConfig {
        width,
        height,
        pixel_ratio: ratio,
        ..RenderConfig::default()
    };
    SkiaBackend::with_text_engine(
        &config,
        TextEngine::with_fonts(fonts).expect("bundled font"),
    )
    .unwrap()
}

fn golden_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(format!("{name}.png"))
}

fn assert_golden(name: &str, backend: &SkiaBackend) {
    let path = golden_path(name);
    let png = backend.to_png().unwrap();
    if std::env::var_os("OSMIC_UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &png).unwrap();
        eprintln!("updated golden {}", path.display());
        return;
    }
    let expected = Pixmap::load_png(&path).unwrap_or_else(|e| {
        panic!(
            "missing or unreadable golden {} ({e}); run with OSMIC_UPDATE_GOLDEN=1 to create it",
            path.display()
        )
    });
    let (w, h) = backend.physical_size();
    let mismatch = if (expected.width(), expected.height()) != (w, h) {
        Some(format!(
            "size {}x{} != golden {}x{}",
            w,
            h,
            expected.width(),
            expected.height()
        ))
    } else {
        let worst = backend
            .premultiplied_pixels()
            .iter()
            .zip(expected.data())
            .map(|(a, b)| (i16::from(*a) - i16::from(*b)).abs())
            .max()
            .unwrap_or(0);
        (worst > CHANNEL_TOLERANCE)
            .then(|| format!("max channel difference {worst} > {CHANNEL_TOLERANCE}"))
    };
    if let Some(why) = mismatch {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("golden-failures");
        std::fs::create_dir_all(&dir).unwrap();
        let actual = dir.join(format!("{name}.png"));
        std::fs::write(&actual, &png).unwrap();
        panic!(
            "{name}: {why}. Actual image: {}. If the change is intended, regenerate with OSMIC_UPDATE_GOLDEN=1.",
            actual.display()
        );
    }
}

fn rgb(hex: &str) -> Color {
    Color::parse(hex).unwrap()
}

/// A fill with a hole, a dashed line, a round-joined polyline, a circle and
/// a haloed label, at device pixel ratio 1.
#[test]
fn primitives() {
    let mut shapes = RenderLayer::new(0);
    // Hole wound the same way as the exterior: only even-odd leaves it empty.
    shapes.push(RenderFeature::Fill {
        coords: vec![
            vec![[10.0, 10.0], [110.0, 10.0], [110.0, 80.0], [10.0, 80.0]],
            vec![[40.0, 28.0], [80.0, 28.0], [80.0, 62.0], [40.0, 62.0]],
        ],
        color: rgb("#8fbf8f"),
    });
    shapes.push(RenderFeature::Stroke {
        coords: vec![[10.0, 100.0], [190.0, 100.0]],
        color: rgb("#c03030"),
        width: 3.0,
        width_next_zoom: 3.0,
        cap: LineCap::Butt,
        join: LineJoin::Miter,
        dash: vec![9.0, 6.0],
    });
    shapes.push(RenderFeature::Stroke {
        coords: vec![[125.0, 70.0], [150.0, 20.0], [175.0, 60.0], [190.0, 15.0]],
        color: rgb("#3050c0"),
        width: 5.0,
        width_next_zoom: 5.0,
        cap: LineCap::Round,
        join: LineJoin::Round,
        dash: vec![],
    });
    shapes.push(RenderFeature::Circle {
        center: [150.0, 85.0],
        radius: 6.0,
        radius_next_zoom: 6.0,
        color: rgb("#f0c330"),
        stroke_color: rgb("#ffffff"),
        stroke_width: 1.5,
    });
    let mut labels = RenderLayer::new(1);
    labels.push(RenderFeature::Label(LabelCandidate {
        text: "Halo Label".into(),
        anchor: LabelAnchor::Point([60.0, 45.0]),
        style: LabelStyle {
            font_size: 14.0,
            color: rgb("#222222"),
            halo_color: rgb("#ffffff"),
            halo_width: 2.0,
            ..LabelStyle::default()
        },
        layer_rank: 0,
        sort_key: 0.0,
    }));
    let mut scene = SceneGraph::new(rgb("#f8f4f0"));
    scene.add_layer(shapes);
    scene.add_layer(labels);

    let mut b = backend(200, 120, 1.0);
    b.render(&scene).unwrap();
    assert_golden("primitives", &b);
}

/// The same scene on a 2x display: geometry, dashes and text all scale.
#[test]
fn primitives_at_device_pixel_ratio_2() {
    let mut layer = RenderLayer::new(0);
    layer.push(RenderFeature::Stroke {
        coords: vec![[5.0, 20.0], [95.0, 20.0]],
        color: rgb("#c03030"),
        width: 3.0,
        width_next_zoom: 3.0,
        cap: LineCap::Butt,
        join: LineJoin::Miter,
        dash: vec![9.0, 6.0],
    });
    layer.push(RenderFeature::Label(LabelCandidate {
        text: "2x".into(),
        anchor: LabelAnchor::Point([50.0, 40.0]),
        style: LabelStyle {
            font_size: 14.0,
            halo_color: rgb("#ffffff"),
            halo_width: 1.5,
            ..LabelStyle::default()
        },
        layer_rank: 0,
        sort_key: 0.0,
    }));
    let mut scene = SceneGraph::new(rgb("#f8f4f0"));
    scene.add_layer(layer);
    let mut b = backend(100, 60, 2.0);
    b.render(&scene).unwrap();
    assert_eq!(b.physical_size(), (200, 120));
    assert_golden("primitives_2x", &b);
}

fn feature(layer: &str, class: &str, name: Option<&str>, geometry: Geometry) -> DecodedFeature {
    DecodedFeature {
        layer: layer.into(),
        id: None,
        class: Some(class.into()),
        name: name.map(str::to_string),
        tags: vec![],
        geometry,
    }
}

/// Style → scene → raster, end to end: land use, water with an island,
/// buildings, roads with casings, a dashed boundary, a POI and labels,
/// including a label that follows a diagonal road.
#[test]
fn default_style_map() {
    let camera = Camera::new(-122.4194, 37.7749, 15.5, 320.0, 240.0);
    let at = |x: f64, y: f64| camera.screen_to_lonlat(x, y);
    let ring = |pts: &[(f64, f64)]| {
        LineString::from(pts.iter().map(|&(x, y)| at(x, y)).collect::<Vec<_>>())
    };
    let line = |pts: &[(f64, f64)]| Geometry::Line(ring(pts));
    let poly = |pts: &[(f64, f64)], holes: Vec<LineString<f64>>| {
        Geometry::Polygon(Polygon::new(ring(pts), holes))
    };
    let pt = |x: f64, y: f64| {
        let (lon, lat) = at(x, y);
        Geometry::Point(Point::new(lon, lat))
    };

    let features = vec![
        feature(
            "landuse",
            "forest",
            None,
            poly(
                &[
                    (0.0, 0.0),
                    (140.0, 0.0),
                    (140.0, 90.0),
                    (0.0, 90.0),
                    (0.0, 0.0),
                ],
                vec![],
            ),
        ),
        feature(
            "leisure",
            "park",
            Some("Mission Park"),
            poly(
                &[
                    (150.0, 10.0),
                    (310.0, 10.0),
                    (310.0, 80.0),
                    (150.0, 80.0),
                    (150.0, 10.0),
                ],
                vec![],
            ),
        ),
        feature(
            "water",
            "lake",
            Some("Blue Lake"),
            poly(
                &[
                    (10.0, 150.0),
                    (130.0, 150.0),
                    (130.0, 230.0),
                    (10.0, 230.0),
                    (10.0, 150.0),
                ],
                vec![ring(&[
                    (60.0, 180.0),
                    (90.0, 180.0),
                    (90.0, 205.0),
                    (60.0, 205.0),
                    (60.0, 180.0),
                ])],
            ),
        ),
        feature(
            "building",
            "yes",
            None,
            poly(
                &[
                    (200.0, 150.0),
                    (240.0, 150.0),
                    (240.0, 185.0),
                    (200.0, 185.0),
                    (200.0, 150.0),
                ],
                vec![],
            ),
        ),
        feature(
            "building",
            "yes",
            None,
            poly(
                &[
                    (250.0, 160.0),
                    (285.0, 160.0),
                    (285.0, 200.0),
                    (250.0, 200.0),
                    (250.0, 160.0),
                ],
                vec![],
            ),
        ),
        feature(
            "highway",
            "primary",
            Some("Market Street"),
            line(&[(0.0, 120.0), (160.0, 120.0), (320.0, 120.0)]),
        ),
        feature(
            "highway",
            "residential",
            Some("Valencia Avenue"),
            line(&[(140.0, 240.0), (230.0, 140.0), (300.0, 20.0)]),
        ),
        feature(
            "highway",
            "motorway",
            None,
            line(&[(0.0, 100.0), (320.0, 100.0)]),
        ),
        feature(
            "boundary",
            "administrative",
            None,
            line(&[(0.0, 140.0), (100.0, 135.0), (200.0, 140.0), (320.0, 135.0)]),
        ),
        feature("shop", "bakery", Some("Bread & Co"), pt(220.0, 210.0)),
        feature("place", "town", Some("Fogtown"), pt(80.0, 40.0)),
    ];

    let style = default_style();
    let scene = SceneBuilder::new(&style).build(
        &features,
        &SceneOptions::new(camera.zoom(), camera.pixel_mapping())
            .with_cull([-20.0, -20.0, 340.0, 260.0]),
    );
    assert!(scene.feature_count() > 20);

    let mut b = backend(320, 240, 1.0);
    b.render(&scene).unwrap();
    assert_golden("default_style_map", &b);
}

/// Rendering is deterministic: two backends produce identical pixels.
#[test]
fn rendering_is_deterministic() {
    let mut layer = RenderLayer::new(0);
    layer.push(RenderFeature::Label(LabelCandidate {
        text: "Deterministic".into(),
        anchor: LabelAnchor::Line(vec![[5.0, 10.0], [180.0, 40.0]]),
        style: LabelStyle {
            font_size: 13.0,
            halo_color: Color::WHITE,
            halo_width: 1.5,
            ..LabelStyle::default()
        },
        layer_rank: 0,
        sort_key: 0.0,
    }));
    let mut scene = SceneGraph::new(Color::WHITE);
    scene.add_layer(layer);
    let render = || {
        let mut b = backend(200, 60, 1.0);
        b.render(&scene).unwrap();
        b.read_pixels().unwrap()
    };
    let first = render();
    assert!(
        first.as_chunks::<4>().0.iter().any(|p| p[0] < 100),
        "line label drawn"
    );
    assert_eq!(first, render());
}
