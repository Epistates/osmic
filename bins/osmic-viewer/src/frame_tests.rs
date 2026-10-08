//! Whole-frame tests: the pieces the viewer wires together, run headless.
//!
//! Tile data → plan → label overlay → GPU renderer → pixels, with the real
//! default style and the bundled test font. Skipped without a GPU adapter.

use std::collections::HashSet;

use geo_types::{LineString, Point, Polygon};
use osmic_core::{Color, Geometry, TileCoord, mercator};
use osmic_render::Camera;
use osmic_text::TextEngine;
use osmic_tiles::mvt_decode::DecodedFeature;

use crate::loader::TileData;
use crate::overlay::{OverlayInput, TileLabels, render_overlay};
use crate::plan::plan_draws;
use crate::renderer::{Renderer, TileDraw, tile_draw_uniform};

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

#[test]
fn a_loaded_tile_renders_with_style_colors_and_labels() {
    // The viewer's pipeline for one frame on a 640x480 logical window at 1x.
    let size = [640u32, 480];
    let (lon, lat) = (-122.4194, 37.7749);
    let (tx, ty) = mercator::lonlat_to_tile(lon, lat, 14);
    let tile = TileCoord::new(tx, ty, osmic_core::Zoom(14));
    let bb = tile.bbox();
    // Fractions of the tile, y measured down from its top edge.
    let at = |fx: f64, fy: f64| (bb.min_lon + bb.width() * fx, bb.max_lat - bb.height() * fy);
    let ring = |pts: &[(f64, f64)]| {
        LineString::from(pts.iter().map(|&(x, y)| at(x, y)).collect::<Vec<_>>())
    };
    let features = vec![
        feature(
            "landuse",
            "forest",
            None,
            Geometry::Polygon(Polygon::new(
                ring(&[
                    (0.05, 0.05),
                    (0.45, 0.05),
                    (0.45, 0.45),
                    (0.05, 0.45),
                    (0.05, 0.05),
                ]),
                vec![],
            )),
        ),
        feature(
            "highway",
            "primary",
            Some("Main Street"),
            Geometry::Line(ring(&[(0.0, 0.7), (1.0, 0.7)])),
        ),
        feature("place", "town", Some("Fogtown"), {
            let (x, y) = at(0.7, 0.3);
            Geometry::Point(Point::new(x, y))
        }),
    ];

    let style = osmic_style::default_style();
    let data = TileData::build(tile, &features, &style);

    let camera = Camera::new(
        (bb.min_lon + bb.max_lon) / 2.0,
        (bb.min_lat + bb.max_lat) / 2.0,
        14.0,
        f64::from(size[0]),
        f64::from(size[1]),
    );
    let visible = camera.visible_tiles(14, 0.0);
    let loaded: HashSet<TileCoord> = [tile].into();
    let plan = plan_draws(&camera, &visible, |c| loaded.contains(c));
    assert_eq!(plan.len(), 1, "only the loaded tile is drawn");

    let device_and_queue = {
        let instance = wgpu::Instance::default();
        pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
            .ok()
            .and_then(|a| {
                pollster::block_on(a.request_device(&wgpu::DeviceDescriptor::default())).ok()
            })
    };
    let Some((device, queue)) = device_and_queue else {
        eprintln!("no GPU adapter available; skipping headless frame test");
        return;
    };
    let format = wgpu::TextureFormat::Rgba8Unorm;
    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("frame"),
        size: wgpu::Extent3d {
            width: size[0],
            height: size[1],
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let mut renderer = Renderer::new(device, queue, format, false, 4, size);

    let gpu_tile = renderer.upload_tile(&data.mesh).expect("tile has geometry");
    let mut engine = TextEngine::with_fonts([include_bytes!(
        "../../../crates/osmic-text/tests/fonts/Cantarell-Regular.ttf"
    )
    .to_vec()])
    .unwrap();
    let mut overlay = Vec::new();
    render_overlay(
        &mut engine,
        &OverlayInput {
            scale: 1.0,
            size,
            tiles: &[TileLabels {
                transform: plan[0].transform,
                labels: &data.labels,
            }],
            panel: None,
        },
        &mut overlay,
    );
    renderer.upload_overlay(&overlay, size);
    let draws = [TileDraw {
        tile: &gpu_tile,
        uniform: tile_draw_uniform(plan[0].transform, 14, camera.zoom(), camera.size(), false),
        scissor: [0, 0, size[0], size[1]],
    }];
    let background = crate::app::background_color(&style, camera.zoom());
    renderer.draw(
        &target.create_view(&wgpu::TextureViewDescriptor::default()),
        background,
        &draws,
    );

    // Read the frame back.
    let buffer = renderer.device().create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: u64::from(size[0] * size[1] * 4),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = renderer
        .device()
        .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &target,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(size[0] * 4),
                rows_per_image: Some(size[1]),
            },
        },
        wgpu::Extent3d {
            width: size[0],
            height: size[1],
            depth_or_array_layers: 1,
        },
    );
    renderer.queue().submit([encoder.finish()]);
    let (tx, rx) = std::sync::mpsc::channel();
    buffer.slice(..).map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    renderer
        .device()
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");
    rx.recv().unwrap().unwrap();
    let pixels = buffer
        .slice(..)
        .get_mapped_range()
        .expect("mapped")
        .to_vec();

    if let Some(path) = std::env::var_os("OSMIC_DUMP_FRAME") {
        let pixmap = tiny_skia::Pixmap::from_vec(
            pixels.clone(),
            tiny_skia::IntSize::from_wh(size[0], size[1]).unwrap(),
        )
        .unwrap();
        pixmap.save_png(path).unwrap();
    }

    let px = |x: u32, y: u32| {
        let i = ((y * size[0] + x) * 4) as usize;
        [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
    };
    let near = |got: [u8; 4], want: [u8; 3]| {
        got[..3]
            .iter()
            .zip(want)
            .all(|(g, w)| (i32::from(*g) - i32::from(w)).abs() <= 2)
    };
    // Outside the tile: the style's background.
    let bg = Color::parse("#f8f4f0").unwrap().to_rgba8();
    assert!(near(px(5, 5), [bg[0], bg[1], bg[2]]), "{:?}", px(5, 5));
    // Inside the forest polygon: forest green at 0.8 opacity over the
    // background — not washed out, not gamma-shifted.
    let t = plan[0].transform;
    let (fx, fy) = (
        (t.offset[0] + 100.0 * t.scale) as u32,
        (t.offset[1] + 100.0 * t.scale) as u32,
    );
    let expected = [
        (173.0 * 0.8 + 248.0 * 0.2) as u8,
        (209.0 * 0.8 + 244.0 * 0.2) as u8,
        (158.0 * 0.8 + 240.0 * 0.2) as u8,
    ];
    assert!(
        near(px(fx, fy), expected),
        "{:?} vs {expected:?}",
        px(fx, fy)
    );
    // The primary road: its fill color across the tile (clear of the label).
    let road_y = (t.offset[1] + 0.7 * 512.0 * t.scale) as u32;
    assert!(
        near(px(120, road_y), [0xfc, 0xd6, 0xa4]),
        "road pixel {:?}",
        px(120, road_y)
    );
    // Labels were drawn: dark glyph pixels exist in the place label's area.
    let dark = pixels
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|p| p[0] < 90 && p[1] < 90 && p[2] < 90)
        .count();
    assert!(dark > 30, "label text drawn ({dark} dark pixels)");
}
