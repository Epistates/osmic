use std::path::PathBuf;
use std::sync::Arc;

use bytemuck::{Pod, Zeroable};
use clap::Parser;
use cosmic_text::{Attrs, Buffer as TextBuffer, Family, FontSystem, Metrics, Shaping, SwashCache};
use lyon::math::point;
use lyon::path::Path as LyonPath;
use lyon::tessellation::*;
use tracing::info;
use wgpu::util::DeviceExt;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::window::{Window, WindowAttributes, WindowId};

use osmic_core::Color;
use osmic_core::bbox::BBox;
use osmic_geo::projection::bbox_to_tile_range;
use osmic_tiles::mvt_decode::{self, DecodedFeature};

#[derive(Parser)]
#[command(name = "osmic-viewer", about = "Interactive map viewer")]
struct Args {
    /// PMTiles file to view
    pmtiles_file: PathBuf,

    /// Initial center longitude
    #[arg(long, default_value = "-98.5", allow_hyphen_values = true)]
    lon: f64,

    /// Initial center latitude
    #[arg(long, default_value = "39.8")]
    lat: f64,

    /// Initial zoom level
    #[arg(long, default_value = "10")]
    zoom: f64,
}

// --- Vertex format ---

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
struct Vertex {
    position: [f32; 2],
    color: [f32; 4],
}

impl Vertex {
    const LAYOUT: wgpu::VertexBufferLayout<'static> = wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<Vertex>() as u64,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: &[
            wgpu::VertexAttribute {
                offset: 0,
                shader_location: 0,
                format: wgpu::VertexFormat::Float32x2,
            },
            wgpu::VertexAttribute {
                offset: 8,
                shader_location: 1,
                format: wgpu::VertexFormat::Float32x4,
            },
        ],
    };
}

// --- Camera ---

struct Camera {
    center_lon: f64,
    center_lat: f64,
    zoom: f64, // degrees of longitude visible across the window width
}

impl Camera {
    fn new(lon: f64, lat: f64, zoom: f64) -> Self {
        Self {
            center_lon: lon,
            center_lat: lat,
            zoom,
        }
    }

    fn view_bbox(&self, aspect: f64) -> BBox {
        let half_w = self.zoom / 2.0;
        let half_h = half_w / aspect;
        BBox::new(
            self.center_lon - half_w,
            self.center_lat - half_h,
            self.center_lon + half_w,
            self.center_lat + half_h,
        )
    }

    fn projection(&self, aspect: f64) -> [[f32; 4]; 4] {
        let bb = self.view_bbox(aspect);
        orthographic(
            bb.min_lon as f32,
            bb.max_lon as f32,
            bb.min_lat as f32,
            bb.max_lat as f32,
        )
    }

    fn tile_zoom(&self) -> u8 {
        let z = (360.0 / self.zoom).log2();
        (z as u8).clamp(0, 14)
    }
}

fn orthographic(left: f32, right: f32, bottom: f32, top: f32) -> [[f32; 4]; 4] {
    let w = right - left;
    let h = top - bottom;
    [
        [2.0 / w, 0.0, 0.0, 0.0],
        [0.0, 2.0 / h, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [-(right + left) / w, -(top + bottom) / h, 0.0, 1.0],
    ]
}

// --- Tessellation ---

fn tessellate_features(features: &[DecodedFeature], view_degrees: f64) -> (Vec<Vertex>, Vec<u32>) {
    // Scale factor: target pixel widths → world degrees
    // At 1280px wide viewing `view_degrees`, 1 pixel = view_degrees/1280
    let px_to_deg = (view_degrees / 1280.0) as f32;
    let mut vertices: Vec<Vertex> = Vec::new();
    let mut indices: Vec<u32> = Vec::new();

    // Sort by layer z-order. POI layers render on top of everything else.
    let mut sorted: Vec<&DecodedFeature> = features.iter().collect();
    sorted.sort_by_key(|f| match f.layer.as_str() {
        "landuse" => 0,
        "natural" => 1,
        "leisure" => 2,
        "water" => 3,
        "building" => 4,
        "boundary" => 5,
        "railway" => 6,
        "highway" => 7,
        "amenity" | "shop" | "office" | "craft" | "healthcare" | "tourism" | "historic"
        | "place" => 9,
        _ => 8,
    });

    for feature in &sorted {
        let is_area = matches!(
            feature.layer.as_str(),
            "building" | "landuse" | "natural" | "leisure"
        ) || (feature.layer == "water"
            && feature
                .class
                .as_deref()
                .is_some_and(|c| matches!(c, "lake" | "pond" | "reservoir" | "basin")));

        let is_line = matches!(feature.layer.as_str(), "highway" | "railway" | "boundary")
            || (feature.layer == "water"
                && feature
                    .class
                    .as_deref()
                    .is_some_and(|c| matches!(c, "river" | "stream" | "canal")));

        let is_point = matches!(feature.geometry, osmic_core::Geometry::Point(_));

        if is_area && let Some(color) = area_color(&feature.layer, feature.class.as_deref()) {
            tessellate_fill(&feature.geometry, &color, &mut vertices, &mut indices);
        }
        if is_line
            && let Some((color, px_width)) = line_style(&feature.layer, feature.class.as_deref())
        {
            // Convert pixel width to world-space degrees
            let world_width = px_width * px_to_deg;
            tessellate_stroke(
                &feature.geometry,
                &color,
                world_width,
                &mut vertices,
                &mut indices,
            );
        }
        if is_point
            && let Some((color, px_radius)) = point_style(&feature.layer, feature.class.as_deref())
        {
            let world_radius = px_radius * px_to_deg;
            tessellate_point(
                &feature.geometry,
                &color,
                world_radius,
                &mut vertices,
                &mut indices,
            );
        }
    }

    (vertices, indices)
}

fn tessellate_fill(
    geom: &osmic_core::Geometry,
    color: &Color,
    vertices: &mut Vec<Vertex>,
    indices: &mut Vec<u32>,
) {
    let rgba = [color.r, color.g, color.b, color.a];

    let coords = match geom {
        osmic_core::Geometry::Polygon(poly) => {
            let mut rings = vec![];
            rings.push(poly.exterior().coords().collect::<Vec<_>>());
            for hole in poly.interiors() {
                rings.push(hole.coords().collect::<Vec<_>>());
            }
            rings
        }
        _ => return,
    };

    let mut builder = LyonPath::builder();
    for ring in &coords {
        if ring.len() < 3 {
            continue;
        }
        builder.begin(point(ring[0].x as f32, ring[0].y as f32));
        for c in &ring[1..] {
            builder.line_to(point(c.x as f32, c.y as f32));
        }
        builder.end(true);
    }
    let path = builder.build();

    let base = vertices.len() as u32;
    let mut geometry: VertexBuffers<Vertex, u32> = VertexBuffers::new();
    let mut tessellator = FillTessellator::new();
    let result = tessellator.tessellate_path(
        &path,
        &FillOptions::tolerance(0.0001),
        &mut BuffersBuilder::new(&mut geometry, |vertex: FillVertex| Vertex {
            position: vertex.position().to_array(),
            color: rgba,
        }),
    );
    if result.is_ok() {
        vertices.extend_from_slice(&geometry.vertices);
        indices.extend(geometry.indices.iter().map(|i| i + base));
    }
}

fn tessellate_stroke(
    geom: &osmic_core::Geometry,
    color: &Color,
    width: f32,
    vertices: &mut Vec<Vertex>,
    indices: &mut Vec<u32>,
) {
    let rgba = [color.r, color.g, color.b, color.a];

    let line_coords = match geom {
        osmic_core::Geometry::Line(ls) => ls.coords().collect::<Vec<_>>(),
        _ => return,
    };
    if line_coords.len() < 2 {
        return;
    }

    let mut builder = LyonPath::builder();
    builder.begin(point(line_coords[0].x as f32, line_coords[0].y as f32));
    for c in &line_coords[1..] {
        builder.line_to(point(c.x as f32, c.y as f32));
    }
    builder.end(false);
    let path = builder.build();

    let base = vertices.len() as u32;
    let mut geometry: VertexBuffers<Vertex, u32> = VertexBuffers::new();
    let mut tessellator = StrokeTessellator::new();
    let result = tessellator.tessellate_path(
        &path,
        &StrokeOptions::tolerance(0.0001)
            .with_line_width(width)
            .with_line_cap(lyon::tessellation::LineCap::Round)
            .with_line_join(lyon::tessellation::LineJoin::Round),
        &mut BuffersBuilder::new(&mut geometry, |vertex: StrokeVertex| Vertex {
            position: vertex.position().to_array(),
            color: rgba,
        }),
    );
    if result.is_ok() {
        vertices.extend_from_slice(&geometry.vertices);
        indices.extend(geometry.indices.iter().map(|i| i + base));
    }
}

/// Emit a small disc (8 triangles from a 9-vertex fan) centered on the
/// feature's point coordinate. The radius is in world-space degrees —
/// callers pass pixel-space radius × `px_to_deg` so dots stay the same
/// visual size at any zoom level.
fn tessellate_point(
    geom: &osmic_core::Geometry,
    color: &Color,
    radius: f32,
    vertices: &mut Vec<Vertex>,
    indices: &mut Vec<u32>,
) {
    let rgba = [color.r, color.g, color.b, color.a];

    let (cx, cy) = match geom {
        osmic_core::Geometry::Point(p) => (p.x() as f32, p.y() as f32),
        _ => return,
    };

    // 8-segment disc approximation. 9 vertices (1 center + 8 rim),
    // 8 triangles fanning out from the center.
    const SEGMENTS: u32 = 8;

    let base = vertices.len() as u32;
    vertices.push(Vertex {
        position: [cx, cy],
        color: rgba,
    });
    for i in 0..SEGMENTS {
        let theta = (i as f32) * std::f32::consts::TAU / (SEGMENTS as f32);
        vertices.push(Vertex {
            position: [cx + radius * theta.cos(), cy + radius * theta.sin()],
            color: rgba,
        });
    }
    for i in 0..SEGMENTS {
        indices.push(base);
        indices.push(base + 1 + i);
        indices.push(base + 1 + ((i + 1) % SEGMENTS));
    }
}

/// Pixel-space radius and fill color for POI layers. The viewer lacks a
/// real symbol/icon pipeline, so every point renders as a single flat
/// disc keyed by layer name. Returns None for non-POI layers.
fn point_style(layer: &str, class: Option<&str>) -> Option<(Color, f32)> {
    let (hex, radius_px) = match layer {
        "shop" => ("#ac39ac", 3.5), // magenta — retail
        "amenity" => match class.unwrap_or("") {
            "restaurant" | "cafe" | "bar" | "pub" | "fast_food" => ("#d96c22", 3.5),
            "hospital" | "clinic" | "pharmacy" | "doctors" => ("#c8372d", 3.5),
            "school" | "university" | "college" | "kindergarten" => ("#f0c330", 3.0),
            "bank" | "atm" => ("#445566", 3.0),
            "fuel" | "charging_station" | "car_wash" => ("#2878a6", 3.0),
            _ => ("#6f6f6f", 2.5),
        },
        "office" => ("#4a6fa5", 3.0),
        "craft" => ("#8b5a3c", 3.0),
        "healthcare" => ("#c8372d", 3.5),
        "tourism" => ("#3fa34d", 3.5),
        "historic" => ("#7a5c40", 3.0),
        "place" => ("#333333", 4.0),
        _ => return None,
    };
    Some((Color::from_hex(hex)?, radius_px))
}

fn area_color(layer: &str, class: Option<&str>) -> Option<Color> {
    match layer {
        "building" => Color::from_hex("#dfdbd7"),
        "landuse" => match class.unwrap_or("") {
            "forest" => Color::from_hex("#add19e"),
            "grass" | "meadow" => Color::from_hex("#cdebb0"),
            "farmland" => Color::from_hex("#d5e29e"),
            "residential" => Color::from_hex("#e0d6d0"),
            _ => Color::from_hex("#d5cfc8"),
        },
        "natural" => match class.unwrap_or("") {
            "wood" => Color::from_hex("#add19e"),
            "water" => Color::from_hex("#aad3df"),
            "grassland" => Color::from_hex("#cdebb0"),
            _ => None,
        },
        "leisure" => Color::from_hex("#c8facc"),
        "water" => Color::from_hex("#aad3df"),
        _ => None,
    }
}

/// Returns (color, width_in_pixels) for line features.
fn line_style(layer: &str, class: Option<&str>) -> Option<(Color, f32)> {
    match layer {
        "highway" => {
            let (c, w) = match class.unwrap_or("") {
                "motorway" | "motorway_link" => ("#e892a2", 6.0),
                "trunk" | "trunk_link" => ("#f9b29c", 5.0),
                "primary" | "primary_link" => ("#fcd6a4", 4.0),
                "secondary" | "secondary_link" => ("#f7fabf", 3.0),
                "tertiary" | "tertiary_link" => ("#ffffff", 2.5),
                "residential" => ("#ffffff", 1.5),
                _ => ("#cccccc", 1.0),
            };
            Some((Color::from_hex(c).unwrap(), w))
        }
        "railway" => Some((Color::from_hex("#bfbfbf").unwrap(), 2.0)),
        "boundary" => Some((Color::from_hex("#9e9cab").unwrap(), 2.0)),
        "water" => Some((Color::from_hex("#aad3df").unwrap(), 3.0)),
        _ => None,
    }
}

// --- Tile loading ---

fn load_tiles_blocking(path: &std::path::Path, bbox: &BBox, zoom: u8) -> Vec<DecodedFeature> {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let backend = pmtiles::MmapBackend::try_from(path).await.unwrap();
        let reader: pmtiles::AsyncPmTilesReader<pmtiles::MmapBackend> =
            pmtiles::AsyncPmTilesReader::try_from_source(backend)
                .await
                .unwrap();

        let (min_x, min_y, max_x, max_y) = bbox_to_tile_range(bbox, zoom);
        let n = (1u64 << zoom) as f64;
        let mut features = Vec::new();

        for y in min_y..=max_y {
            for x in min_x..=max_x {
                if let Ok(coord) = pmtiles::TileCoord::new(zoom, x, y)
                    && let Ok(Some(data)) = reader.get_tile_decompressed(coord).await
                {
                    features.extend(mvt_decode::decode_tile(&data, zoom, x, y, n));
                }
            }
        }
        features
    })
}

// --- Labels ---

struct MapLabel {
    lon: f64,
    lat: f64,
    text: String,
    font_size: f32,
    color: [u8; 4],
    /// Maximum camera zoom (degrees visible) at which this label appears.
    max_view_deg: f64,
    /// Feature bounding box for re-centering when label scrolls off-screen.
    feature_bbox: BBox,
}

fn collect_labels(features: &[DecodedFeature]) -> Vec<MapLabel> {
    let mut labels = Vec::new();
    for f in features {
        let name = match &f.name {
            Some(n) if !n.is_empty() => n,
            _ => continue,
        };

        // (font_size, color, max_view_deg) - labels appear when camera.zoom <= max_view_deg
        // Wider values = visible when more zoomed out
        let (font_size, color, max_view_deg) = match f.layer.as_str() {
            "highway" => match f.class.as_deref().unwrap_or("") {
                "motorway" | "trunk" => (11.0, [0x55, 0x55, 0x55, 0xFF], 20.0),
                "primary" => (10.0, [0x55, 0x55, 0x55, 0xFF], 5.0),
                "secondary" | "tertiary" => (9.0, [0x66, 0x66, 0x66, 0xFF], 1.0),
                "residential" | "unclassified" => (8.5, [0x77, 0x77, 0x77, 0xFF], 0.2),
                _ => (8.0, [0x77, 0x77, 0x77, 0xFF], 0.1),
            },
            "water" => (13.0, [0x6b, 0x9d, 0xaf, 0xFF], 10.0),
            "place" => match f.class.as_deref().unwrap_or("") {
                "city" => (24.0, [0x33, 0x33, 0x33, 0xFF], 360.0),
                "town" => (18.0, [0x44, 0x44, 0x44, 0xFF], 30.0),
                "village" => (14.0, [0x55, 0x55, 0x55, 0xFF], 5.0),
                _ => (12.0, [0x66, 0x66, 0x66, 0xFF], 2.0),
            },
            "leisure" => (10.0, [0x3a, 0x7a, 0x3a, 0xFF], 1.0),
            "amenity" => (9.0, [0x73, 0x4a, 0x08, 0xFF], 0.05),
            "shop" => (9.0, [0x5b, 0x3a, 0x0a, 0xFF], 0.05),
            "tourism" => (9.0, [0x0d, 0x73, 0x77, 0xFF], 0.1),
            "healthcare" => (9.0, [0xc4, 0x28, 0x1c, 0xFF], 0.05),
            "office" => (8.0, [0x55, 0x55, 0x55, 0xFF], 0.05),
            "craft" => (8.0, [0xb5, 0x65, 0x1d, 0xFF], 0.05),
            "historic" => (9.0, [0x7b, 0x2d, 0x8b, 0xFF], 0.1),
            "club" => (8.0, [0x55, 0x55, 0x88, 0xFF], 0.05),
            "emergency" => (9.0, [0xcc, 0x00, 0x00, 0xFF], 0.1),
            "education" => (9.0, [0x33, 0x66, 0x99, 0xFF], 0.1),
            _ => continue,
        };

        let bb = f.geometry.bbox();
        let center = bb.center();

        labels.push(MapLabel {
            lon: center.lon,
            lat: center.lat,
            text: name.clone(),
            font_size,
            color,
            max_view_deg,
            feature_bbox: bb,
        });
    }
    labels
}

fn render_labels_to_rgba(
    labels: &[MapLabel],
    camera: &Camera,
    width: u32,
    height: u32,
    font_system: &mut FontSystem,
    swash_cache: &mut SwashCache,
    info_panel: Option<&InfoPanel>,
) -> Vec<u8> {
    let aspect = width as f64 / height as f64;
    let bb = camera.view_bbox(aspect);
    let w = width as f64;
    let h = height as f64;

    let mut pixels = vec![0u8; (width * height * 4) as usize];

    // Grid-based collision detection: divide screen into cells
    let cell_w = 120.0f32; // pixels per cell
    let cell_h = 30.0f32;
    let grid_cols = (width as f32 / cell_w).ceil() as usize + 1;
    let grid_rows = (height as f32 / cell_h).ceil() as usize + 1;
    let mut occupied = vec![false; grid_cols * grid_rows];

    // Sort labels by priority (larger font = higher priority)
    let mut sorted_indices: Vec<usize> = (0..labels.len()).collect();
    sorted_indices.sort_by(|&a, &b| {
        labels[b]
            .font_size
            .partial_cmp(&labels[a].font_size)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut placed = 0;
    let max_labels = 500; // cap for performance

    for &idx in &sorted_indices {
        if placed >= max_labels {
            break;
        }
        let label = &labels[idx];

        // Skip labels not visible at current zoom
        if camera.zoom > label.max_view_deg {
            continue;
        }

        // Convert lon/lat to pixel
        let mut px = ((label.lon - bb.min_lon) / bb.width() * w) as f32;
        let mut py = ((bb.max_lat - label.lat) / bb.height() * h) as f32;

        // If label center is off-screen but feature is still visible, re-center
        let margin = 50.0f32;
        let on_screen =
            px >= -margin && py >= -margin && px <= w as f32 + margin && py <= h as f32 + margin;

        if !on_screen {
            // Check if the feature bbox overlaps the viewport
            if !label.feature_bbox.intersects(&bb) {
                continue;
            }
            // Clamp the label to the visible portion of the feature
            let vis_lon = label
                .lon
                .clamp(bb.min_lon + bb.width() * 0.1, bb.max_lon - bb.width() * 0.1);
            let vis_lat = label.lat.clamp(
                bb.min_lat + bb.height() * 0.1,
                bb.max_lat - bb.height() * 0.1,
            );
            px = ((vis_lon - bb.min_lon) / bb.width() * w) as f32;
            py = ((bb.max_lat - vis_lat) / bb.height() * h) as f32;
        }

        // Estimate text bounds for collision
        let est_w = label.text.len() as f32 * label.font_size * 0.55;
        let est_h = label.font_size * 1.3;
        let x0 = px - est_w / 2.0;
        let y0 = py - est_h / 2.0;

        // Check collision grid
        let col_min = ((x0 / cell_w).floor() as isize).max(0) as usize;
        let col_max = (((x0 + est_w) / cell_w).ceil() as usize).min(grid_cols - 1);
        let row_min = ((y0 / cell_h).floor() as isize).max(0) as usize;
        let row_max = (((y0 + est_h) / cell_h).ceil() as usize).min(grid_rows - 1);

        let mut collides = false;
        for row in row_min..=row_max {
            for col in col_min..=col_max {
                if occupied[row * grid_cols + col] {
                    collides = true;
                    break;
                }
            }
            if collides {
                break;
            }
        }
        if collides {
            continue;
        }

        // Mark cells as occupied
        for row in row_min..=row_max {
            for col in col_min..=col_max {
                occupied[row * grid_cols + col] = true;
            }
        }

        // Scale font size based on zoom: labels grow as you zoom in.
        // base_zoom_deg is where the label first appears (max_view_deg).
        // At that zoom the label is at its base font_size; zooming in 2x doubles it.
        let zoom_scale = (label.max_view_deg / camera.zoom).sqrt().clamp(1.0, 4.0) as f32;
        let effective_size = (label.font_size * zoom_scale).clamp(8.0, 40.0);

        // Shape and render this label
        let metrics = Metrics::new(effective_size, effective_size * 1.2);
        let mut buffer = TextBuffer::new(font_system, metrics);
        let attrs = Attrs::new().family(Family::SansSerif);
        buffer.set_text(font_system, &label.text, &attrs, Shaping::Advanced, None);
        buffer.shape_until_scroll(font_system, false);

        let mut text_w: f32 = 0.0;
        for run in buffer.layout_runs() {
            text_w = text_w.max(run.line_w);
        }
        let offset_x = px - text_w / 2.0;
        let offset_y = py - effective_size / 2.0;

        // Halo
        let halo = cosmic_text::Color::rgba(255, 255, 255, 220);
        for &dy in &[-1.0f32, 0.0, 1.0] {
            for &dx in &[-1.0f32, 0.0, 1.0] {
                if dx == 0.0 && dy == 0.0 {
                    continue;
                }
                draw_text_to_buf(
                    &buffer,
                    font_system,
                    swash_cache,
                    &mut pixels,
                    width,
                    height,
                    offset_x + dx,
                    offset_y + dy,
                    halo,
                );
            }
        }

        // Text
        let [r, g, b, a] = label.color;
        draw_text_to_buf(
            &buffer,
            font_system,
            swash_cache,
            &mut pixels,
            width,
            height,
            offset_x,
            offset_y,
            cosmic_text::Color::rgba(r, g, b, a),
        );

        placed += 1;
    }

    // Render info panel overlay if present
    if let Some(panel) = info_panel {
        render_info_panel(panel, &mut pixels, width, height, font_system, swash_cache);
    }

    pixels
}

/// Render an info panel as a semi-transparent box with text lines.
fn render_info_panel(
    panel: &InfoPanel,
    pixels: &mut [u8],
    width: u32,
    height: u32,
    font_system: &mut FontSystem,
    swash_cache: &mut SwashCache,
) {
    let font_size = 36.0f32;
    let line_height = (font_size * 1.5) as u32;
    let padding = 24u32;
    let panel_w = 800u32;
    let panel_h = padding * 2 + (panel.lines.len() as u32) * line_height;

    // Position panel near click, clamped to screen
    let mut px = panel.screen_x as u32;
    let mut py = panel.screen_y as u32;
    if px + panel_w + 10 > width {
        px = px.saturating_sub(panel_w + 10);
    } else {
        px += 10;
    }
    if py + panel_h + 10 > height {
        py = py.saturating_sub(panel_h + 10);
    }

    // Draw semi-transparent dark background
    for row in py..py.saturating_add(panel_h).min(height) {
        for col in px..px.saturating_add(panel_w).min(width) {
            let idx = ((row * width + col) * 4) as usize;
            if idx + 3 < pixels.len() {
                // Dark background with alpha blending
                pixels[idx] = 30;
                pixels[idx + 1] = 30;
                pixels[idx + 2] = 40;
                pixels[idx + 3] = 220;
            }
        }
    }

    // Draw a thin border
    let border_color: [u8; 4] = [100, 140, 200, 255];
    for col in px..px.saturating_add(panel_w).min(width) {
        for &row in &[py, py.saturating_add(panel_h).saturating_sub(1)] {
            if row < height {
                let idx = ((row * width + col) * 4) as usize;
                if idx + 3 < pixels.len() {
                    pixels[idx..idx + 4].copy_from_slice(&border_color);
                }
            }
        }
    }
    for row in py..py.saturating_add(panel_h).min(height) {
        for &col in &[px, px.saturating_add(panel_w).saturating_sub(1)] {
            if col < width {
                let idx = ((row * width + col) * 4) as usize;
                if idx + 3 < pixels.len() {
                    pixels[idx..idx + 4].copy_from_slice(&border_color);
                }
            }
        }
    }

    // Render text lines
    let text_x = (px + padding) as f32;
    let mut text_y = (py + padding) as f32;

    for (i, (label, value)) in panel.lines.iter().enumerate() {
        let text = if i == 0 {
            // First line (name) — larger, bold-ish
            value.clone()
        } else {
            format!("{}: {}", label, value)
        };

        let size = if i == 0 { font_size + 10.0 } else { font_size };
        let metrics = Metrics::new(size, size * 1.3);
        let mut buffer = TextBuffer::new(font_system, metrics);
        let attrs = Attrs::new().family(Family::SansSerif);
        buffer.set_text(font_system, &text, &attrs, Shaping::Advanced, None);
        buffer.shape_until_scroll(font_system, false);

        let text_color = if i == 0 {
            cosmic_text::Color::rgba(255, 255, 255, 255)
        } else {
            cosmic_text::Color::rgba(200, 210, 220, 255)
        };

        draw_text_to_buf(
            &buffer,
            font_system,
            swash_cache,
            pixels,
            width,
            height,
            text_x,
            text_y,
            text_color,
        );
        text_y += line_height as f32;
    }
}

// Binary utility: all args are independent rendering inputs; a struct would add ceremony with no benefit.
#[allow(clippy::too_many_arguments)]
fn draw_text_to_buf(
    buffer: &TextBuffer,
    font_system: &mut FontSystem,
    swash_cache: &mut SwashCache,
    pixels: &mut [u8],
    buf_w: u32,
    buf_h: u32,
    offset_x: f32,
    offset_y: f32,
    color: cosmic_text::Color,
) {
    buffer.draw(font_system, swash_cache, color, |x, y, w, h, c| {
        let alpha = c.a();
        if alpha == 0 {
            return;
        }
        for dy in 0..h as i32 {
            for dx in 0..w as i32 {
                let px = (x + dx) + offset_x as i32;
                let py = (y + dy) + offset_y as i32;
                if px >= 0 && py >= 0 && (px as u32) < buf_w && (py as u32) < buf_h {
                    let idx = ((py as u32 * buf_w + px as u32) * 4) as usize;
                    let sa = alpha as f32 / 255.0;
                    pixels[idx] =
                        ((c.r() as f32 * sa + pixels[idx] as f32 * (1.0 - sa)).min(255.0)) as u8;
                    pixels[idx + 1] = ((c.g() as f32 * sa + pixels[idx + 1] as f32 * (1.0 - sa))
                        .min(255.0)) as u8;
                    pixels[idx + 2] = ((c.b() as f32 * sa + pixels[idx + 2] as f32 * (1.0 - sa))
                        .min(255.0)) as u8;
                    pixels[idx + 3] = ((sa + pixels[idx + 3] as f32 / 255.0 * (1.0 - sa)) * 255.0)
                        .min(255.0) as u8;
                }
            }
        }
    });
}

// --- App ---

struct MapApp {
    pmtiles_path: PathBuf,
    camera: Camera,
    font_system: FontSystem,
    swash_cache: SwashCache,
    labels: Vec<MapLabel>,
    /// Decoded features for click-to-inspect
    click_features: Vec<ClickFeature>,
    // wgpu state (initialized on resume)
    gpu: Option<GpuState>,
    // input state
    dragging: bool,
    drag_distance: f64,
    last_cursor: PhysicalPosition<f64>,
    needs_retessellate: bool,
    current_tile_zoom: u8,
    /// Bbox of the area we've loaded tiles for
    loaded_bbox: BBox,
    /// Info panel shown on click (None = hidden)
    info_panel: Option<InfoPanel>,
    /// Whether the info panel overlay needs re-rendering
    info_panel_dirty: bool,
}

/// On-screen info panel for a clicked feature.
struct InfoPanel {
    lines: Vec<(String, String)>, // (label, value) pairs
    screen_x: f32,
    screen_y: f32,
}

/// Lightweight feature data for click-to-inspect.
struct ClickFeature {
    lon: f64,
    lat: f64,
    layer: String,
    class: Option<String>,
    name: Option<String>,
    tags: Vec<(String, String)>,
}

struct GpuState {
    window: Arc<Window>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    uniform_buffer: wgpu::Buffer,
    uniform_bind_group: wgpu::BindGroup,
    vertex_buffer: wgpu::Buffer,
    index_buffer: wgpu::Buffer,
    num_indices: u32,
    // Label overlay
    label_pipeline: wgpu::RenderPipeline,
    label_texture: wgpu::Texture,
    label_bind_group: wgpu::BindGroup,
    label_uniform_buffer: wgpu::Buffer,
    /// Camera lon/lat when labels were last rendered
    label_camera_lon: f64,
    label_camera_lat: f64,
    label_camera_zoom: f64,
}

impl MapApp {
    fn new(args: &Args) -> Self {
        let degrees = 360.0 / 2.0f64.powf(args.zoom);
        Self {
            pmtiles_path: args.pmtiles_file.clone(),
            camera: Camera::new(args.lon, args.lat, degrees),
            font_system: FontSystem::new(),
            swash_cache: SwashCache::new(),
            labels: Vec::new(),
            click_features: Vec::new(),
            gpu: None,
            dragging: false,
            drag_distance: 0.0,
            last_cursor: PhysicalPosition::new(0.0, 0.0),
            needs_retessellate: true,
            current_tile_zoom: 0,
            loaded_bbox: BBox::empty(),
            info_panel: None,
            info_panel_dirty: false,
        }
    }

    fn handle_click(&mut self, cursor: PhysicalPosition<f64>) {
        let gpu = match &self.gpu {
            Some(g) => g,
            None => return,
        };
        let aspect = gpu.config.width as f64 / gpu.config.height as f64;
        let bb = self.camera.view_bbox(aspect);
        let w = gpu.config.width as f64;
        let h = gpu.config.height as f64;

        // Convert pixel position to lon/lat
        let click_lon = bb.min_lon + (cursor.x / w) * bb.width();
        let click_lat = bb.max_lat - (cursor.y / h) * bb.height();

        // Find nearest POI within a reasonable radius
        let search_radius = self.camera.zoom * 0.02; // ~2% of view width
        let cos_lat = (click_lat.to_radians()).cos();
        let mut best: Option<(f64, usize)> = None;

        for (i, f) in self.click_features.iter().enumerate() {
            let dlat = f.lat - click_lat;
            let dlon = (f.lon - click_lon) * cos_lat; // correct for latitude
            let dist = dlat * dlat + dlon * dlon;
            if dist < search_radius * search_radius && (best.is_none() || dist < best.unwrap().0) {
                best = Some((dist, i));
            }
        }

        if let Some((_, idx)) = best {
            let feature = &self.click_features[idx];
            let mut lines: Vec<(String, String)> = Vec::new();

            if let Some(ref name) = feature.name {
                lines.push(("Name".into(), name.clone()));
            }
            if let Some(ref class) = feature.class {
                let pretty = format!("{} / {}", feature.layer, class);
                lines.push(("Type".into(), pretty));
            }

            // Build address
            let num = feature
                .tags
                .iter()
                .find(|(k, _)| k == "addr:housenumber")
                .map(|(_, v)| v.as_str());
            let street = feature
                .tags
                .iter()
                .find(|(k, _)| k == "addr:street")
                .map(|(_, v)| v.as_str());
            let city = feature
                .tags
                .iter()
                .find(|(k, _)| k == "addr:city")
                .map(|(_, v)| v.as_str());
            let postcode = feature
                .tags
                .iter()
                .find(|(k, _)| k == "addr:postcode")
                .map(|(_, v)| v.as_str());
            let mut addr = String::new();
            if let Some(n) = num {
                addr.push_str(n);
                addr.push(' ');
            }
            if let Some(s) = street {
                addr.push_str(s);
            }
            if let Some(c) = city {
                if !addr.is_empty() {
                    addr.push_str(", ");
                }
                addr.push_str(c);
            }
            if let Some(p) = postcode {
                if !addr.is_empty() {
                    addr.push(' ');
                }
                addr.push_str(p);
            }
            if !addr.is_empty() {
                lines.push(("Address".into(), addr));
            }

            // Other useful tags
            let tag_labels = &[
                ("phone", "Phone"),
                ("contact:phone", "Phone"),
                ("website", "Website"),
                ("contact:website", "Website"),
                ("opening_hours", "Hours"),
                ("cuisine", "Cuisine"),
                ("brand", "Brand"),
                ("operator", "Operator"),
                ("description", "Info"),
            ];
            for &(tag_key, label) in tag_labels {
                if let Some((_, v)) = feature.tags.iter().find(|(k, _)| k == tag_key) {
                    lines.push((label.into(), v.clone()));
                }
            }

            lines.push((
                "Location".into(),
                format!("{:.6}, {:.6}", feature.lat, feature.lon),
            ));

            // Cap lines to prevent panel overflow
            lines.truncate(12);

            self.info_panel = Some(InfoPanel {
                lines,
                screen_x: cursor.x as f32,
                screen_y: cursor.y as f32,
            });
            self.info_panel_dirty = true;
        } else {
            // Click on empty area — dismiss panel
            if self.info_panel.is_some() {
                self.info_panel = None;
                self.info_panel_dirty = true;
            }
        }
    }

    fn load_and_tessellate(&mut self) {
        let gpu = self.gpu.as_ref().unwrap();
        let aspect = gpu.config.width as f64 / gpu.config.height as f64;
        let view_bbox = self.camera.view_bbox(aspect);
        // Load tiles with 50% buffer around viewport for smooth panning
        let bw = view_bbox.width() * 0.5;
        let bh = view_bbox.height() * 0.5;
        let load_bbox = BBox::new(
            view_bbox.min_lon - bw,
            view_bbox.min_lat - bh,
            view_bbox.max_lon + bw,
            view_bbox.max_lat + bh,
        );
        let tile_zoom = self.camera.tile_zoom();

        info!(zoom = tile_zoom, bbox = %load_bbox, "Loading tiles");
        let mut features = load_tiles_blocking(&self.pmtiles_path, &load_bbox, tile_zoom);

        // Cap features to prevent GPU buffer overflow (268MB limit ≈ 10M vertices)
        const MAX_FEATURES: usize = 500_000;
        if features.len() > MAX_FEATURES {
            // Keep the most important features: sort by layer priority
            features.sort_by_key(|f| match f.layer.as_str() {
                "boundary" => 0,
                "water" => 1,
                "natural" => 2,
                "landuse" => 3,
                "highway" => match f.class.as_deref().unwrap_or("") {
                    "motorway" | "motorway_link" => 4,
                    "trunk" | "trunk_link" => 5,
                    "primary" | "primary_link" => 6,
                    _ => 8,
                },
                "railway" => 7,
                _ => 9,
            });
            features.truncate(MAX_FEATURES);
            info!(truncated_to = MAX_FEATURES, "Feature budget applied");
        }
        info!(features = features.len(), "Tessellating");

        self.labels = collect_labels(&features);
        info!(labels = self.labels.len(), "Labels collected");

        // Collect clickable POI features (only named POIs to keep it manageable)
        self.click_features = features
            .iter()
            .filter(|f| {
                f.name.is_some()
                    && matches!(
                        f.layer.as_str(),
                        "amenity"
                            | "shop"
                            | "tourism"
                            | "office"
                            | "healthcare"
                            | "craft"
                            | "historic"
                            | "leisure"
                            | "club"
                            | "emergency"
                            | "education"
                    )
            })
            .map(|f| {
                let bb = f.geometry.bbox();
                let center = bb.center();
                ClickFeature {
                    lon: center.lon,
                    lat: center.lat,
                    layer: f.layer.clone(),
                    class: f.class.clone(),
                    name: f.name.clone(),
                    tags: f.tags.clone(),
                }
            })
            .collect();
        info!(
            clickable = self.click_features.len(),
            "Click features indexed"
        );

        let (mut vertices, mut indices) = tessellate_features(&features, self.camera.zoom);

        // Cap vertex/index buffers to stay within wgpu's 268MB limit
        // Vertex = 24 bytes, so 8M vertices = 192MB
        const MAX_VERTICES: usize = 8_000_000;
        const MAX_INDICES: usize = 24_000_000;
        if vertices.len() > MAX_VERTICES {
            vertices.truncate(MAX_VERTICES);
            // Find the last valid triangle boundary in indices
            let max_idx = MAX_INDICES.min(indices.len());
            let trimmed = (max_idx / 3) * 3; // align to triangle boundaries
            indices.truncate(trimmed);
            // Remove indices that reference truncated vertices
            indices.retain(|&i| (i as usize) < MAX_VERTICES);
            let trimmed = (indices.len() / 3) * 3;
            indices.truncate(trimmed);
            info!(
                vertices = vertices.len(),
                indices = indices.len(),
                "Buffer capped"
            );
        }

        info!(
            vertices = vertices.len(),
            indices = indices.len(),
            "GPU buffers ready"
        );

        // Recreate GPU buffers
        let gpu = self.gpu.as_mut().unwrap();

        if vertices.is_empty() {
            gpu.num_indices = 0;
            return;
        }

        gpu.vertex_buffer = gpu
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("Vertex Buffer"),
                contents: bytemuck::cast_slice(&vertices),
                usage: wgpu::BufferUsages::VERTEX,
            });
        gpu.index_buffer = gpu
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("Index Buffer"),
                contents: bytemuck::cast_slice(&indices),
                usage: wgpu::BufferUsages::INDEX,
            });
        gpu.num_indices = indices.len() as u32;
        self.current_tile_zoom = tile_zoom;
        let aspect = gpu.config.width as f64 / gpu.config.height as f64;
        self.loaded_bbox = self.camera.view_bbox(aspect);
        self.needs_retessellate = false;
        self.update_label_texture();
    }

    fn update_label_texture(&mut self) {
        let (w, h) = match &self.gpu {
            Some(g) => (g.config.width, g.config.height),
            None => return,
        };

        let label_pixels = render_labels_to_rgba(
            &self.labels,
            &self.camera,
            w,
            h,
            &mut self.font_system,
            &mut self.swash_cache,
            self.info_panel.as_ref(),
        );

        let gpu = self.gpu.as_mut().unwrap();
        gpu.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &gpu.label_texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &label_pixels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(w * 4),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );

        // Store camera state for pan tracking
        gpu.label_camera_lon = self.camera.center_lon;
        gpu.label_camera_lat = self.camera.center_lat;
        gpu.label_camera_zoom = self.camera.zoom;

        // Reset offset
        gpu.queue.write_buffer(
            &gpu.label_uniform_buffer,
            0,
            bytemuck::cast_slice(&[0.0f32, 0.0f32]),
        );
    }

    fn update_label_offset(&self) {
        if let Some(gpu) = &self.gpu {
            // Compute UV offset: how much camera moved since labels were rendered
            let dx_deg = self.camera.center_lon - gpu.label_camera_lon;
            let dy_deg = self.camera.center_lat - gpu.label_camera_lat;
            let zoom = gpu.label_camera_zoom;
            if zoom > 0.0 {
                let aspect = gpu.config.width as f64 / gpu.config.height as f64;
                let u_offset = (dx_deg / zoom) as f32;
                let v_offset = -(dy_deg / (zoom / aspect)) as f32;
                gpu.queue.write_buffer(
                    &gpu.label_uniform_buffer,
                    0,
                    bytemuck::cast_slice(&[u_offset, v_offset]),
                );
            }
        }
    }

    fn update_projection(&self) {
        if let Some(gpu) = &self.gpu {
            let aspect = gpu.config.width as f64 / gpu.config.height as f64;
            let proj = self.camera.projection(aspect);
            gpu.queue
                .write_buffer(&gpu.uniform_buffer, 0, bytemuck::cast_slice(&proj));
        }
    }

    fn render(&self) {
        let gpu = match &self.gpu {
            Some(g) => g,
            None => return,
        };

        let frame = match gpu.surface.get_current_texture() {
            Ok(f) => f,
            Err(_) => return,
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Map Pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.973,
                            g: 0.957,
                            b: 0.941,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                ..Default::default()
            });
            pass.set_pipeline(&gpu.pipeline);
            pass.set_bind_group(0, &gpu.uniform_bind_group, &[]);
            if gpu.num_indices > 0 {
                pass.set_vertex_buffer(0, gpu.vertex_buffer.slice(..));
                pass.set_index_buffer(gpu.index_buffer.slice(..), wgpu::IndexFormat::Uint32);
                pass.draw_indexed(0..gpu.num_indices, 0, 0..1);
            }

            // Draw label overlay
            pass.set_pipeline(&gpu.label_pipeline);
            pass.set_bind_group(0, &gpu.label_bind_group, &[]);
            pass.draw(0..4, 0..1);
        }
        gpu.queue.submit(std::iter::once(encoder.finish()));
        frame.present();
    }
}

impl ApplicationHandler for MapApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.gpu.is_some() {
            return;
        }

        let window = Arc::new(
            event_loop
                .create_window(
                    WindowAttributes::default()
                        .with_title("Osmic Viewer")
                        .with_inner_size(LogicalSize::new(1280, 960)),
                )
                .unwrap(),
        );

        let size = window.inner_size();
        let instance = wgpu::Instance::default();
        let surface = instance.create_surface(window.clone()).unwrap();
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            compatible_surface: Some(&surface),
            ..Default::default()
        }))
        .unwrap();

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("Map Device"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            ..Default::default()
        }))
        .unwrap();

        let config = surface
            .get_default_config(&adapter, size.width.max(1), size.height.max(1))
            .unwrap();
        surface.configure(&device, &config);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("Map Shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
        });

        let uniform_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Uniform Buffer"),
            contents: bytemuck::cast_slice(&[[0.0f32; 4]; 4]),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });

        let uniform_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("Uniform BGL"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                }],
            });

        let uniform_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("Uniform BG"),
            layout: &uniform_bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buffer.as_entire_binding(),
            }],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("Map Pipeline Layout"),
            bind_group_layouts: &[&uniform_bind_group_layout],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("Map Pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[Vertex::LAYOUT],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: config.format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        // Placeholder buffers
        let vertex_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Vertex Buffer"),
            contents: &[0u8; 24],
            usage: wgpu::BufferUsages::VERTEX,
        });
        let index_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Index Buffer"),
            contents: &[0u8; 4],
            usage: wgpu::BufferUsages::INDEX,
        });

        // --- Label overlay pipeline ---
        let label_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("Label Shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("label_shader.wgsl").into()),
        });

        let label_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("Label Texture"),
            size: wgpu::Extent3d {
                width: size.width.max(1),
                height: size.height.max(1),
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let label_texture_view = label_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let label_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let label_uniform_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Label Uniform Buffer"),
            contents: bytemuck::cast_slice(&[0.0f32, 0.0f32]),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });

        let label_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("Label BGL"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 2,
                        visibility: wgpu::ShaderStages::VERTEX,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                ],
            });

        let label_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("Label BG"),
            layout: &label_bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&label_texture_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&label_sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: label_uniform_buffer.as_entire_binding(),
                },
            ],
        });

        let label_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("Label Pipeline Layout"),
                bind_group_layouts: &[&label_bind_group_layout],
                immediate_size: 0,
            });

        let label_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("Label Pipeline"),
            layout: Some(&label_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &label_shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &label_shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: config.format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleStrip,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        self.gpu = Some(GpuState {
            window,
            device,
            queue,
            surface,
            config,
            pipeline,
            uniform_buffer,
            uniform_bind_group,
            vertex_buffer,
            index_buffer,
            num_indices: 0,
            label_pipeline,
            label_texture,
            label_bind_group,
            label_uniform_buffer,
            label_camera_lon: 0.0,
            label_camera_lat: 0.0,
            label_camera_zoom: 1.0,
        });

        self.load_and_tessellate();
        self.update_projection();
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),

            WindowEvent::Resized(size) => {
                if let Some(gpu) = &mut self.gpu {
                    gpu.config.width = size.width.max(1);
                    gpu.config.height = size.height.max(1);
                    gpu.surface.configure(&gpu.device, &gpu.config);
                }
                self.update_projection();
                if let Some(gpu) = &self.gpu {
                    gpu.window.request_redraw();
                }
            }

            WindowEvent::MouseInput {
                state,
                button: MouseButton::Left,
                ..
            } => {
                let was_dragging = self.dragging;
                if state == ElementState::Pressed {
                    self.dragging = true;
                    self.drag_distance = 0.0;
                } else {
                    self.dragging = false;
                }
                // Reload tiles when drag ends if we've panned outside loaded area
                if was_dragging && !self.dragging {
                    // Click (not drag) — find nearest feature
                    if self.drag_distance < 5.0 {
                        self.handle_click(self.last_cursor);
                        if self.info_panel_dirty {
                            self.update_label_texture();
                            self.info_panel_dirty = false;
                        }
                    }
                    let aspect = self
                        .gpu
                        .as_ref()
                        .map(|g| g.config.width as f64 / g.config.height as f64)
                        .unwrap_or(1.333);
                    let view = self.camera.view_bbox(aspect);
                    // Reload if view extends 30%+ outside loaded area
                    let margin = self.loaded_bbox.width() * 0.3;
                    if view.min_lon < self.loaded_bbox.min_lon - margin
                        || view.max_lon > self.loaded_bbox.max_lon + margin
                        || view.min_lat < self.loaded_bbox.min_lat - margin
                        || view.max_lat > self.loaded_bbox.max_lat + margin
                        || !self.loaded_bbox.is_valid()
                    {
                        self.needs_retessellate = true;
                    }
                    self.update_label_texture();
                    if let Some(gpu) = &self.gpu {
                        gpu.window.request_redraw();
                    }
                }
            }

            WindowEvent::CursorMoved { position, .. } => {
                if self.dragging {
                    let dx = position.x - self.last_cursor.x;
                    let dy = position.y - self.last_cursor.y;
                    self.drag_distance += (dx * dx + dy * dy).sqrt();
                    // Dismiss info panel while dragging
                    if self.info_panel.is_some() {
                        self.info_panel = None;
                    }
                    let (w, h) = self
                        .gpu
                        .as_ref()
                        .map(|g| (g.config.width as f64, g.config.height as f64))
                        .unwrap_or((1.0, 1.0));

                    let dx = position.x - self.last_cursor.x;
                    let dy = position.y - self.last_cursor.y;

                    self.camera.center_lon -= dx / w * self.camera.zoom;
                    self.camera.center_lat += dy / h * self.camera.zoom * (h / w);

                    self.update_projection();
                    self.update_label_offset();

                    let new_tz = self.camera.tile_zoom();
                    if new_tz != self.current_tile_zoom {
                        self.needs_retessellate = true;
                    }

                    if let Some(gpu) = &self.gpu {
                        gpu.window.request_redraw();
                    }
                }
                self.last_cursor = position;
            }

            WindowEvent::MouseWheel { delta, .. } => {
                let scroll = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y as f64,
                    MouseScrollDelta::PixelDelta(p) => p.y / 50.0,
                };
                let factor = 1.0 - scroll * 0.1;
                self.camera.zoom = (self.camera.zoom * factor).clamp(0.001, 360.0);
                self.update_projection();

                // Dismiss info panel on zoom
                self.info_panel = None;

                // Reload tiles when zoom level changes or viewport doubles/halves
                let new_tz = self.camera.tile_zoom();
                if new_tz != self.current_tile_zoom {
                    self.needs_retessellate = true;
                }

                // Re-render labels at new zoom (cheap — CPU only)
                self.update_label_texture();

                if let Some(gpu) = &self.gpu {
                    gpu.window.request_redraw();
                }
            }

            WindowEvent::RedrawRequested => {
                if self.needs_retessellate {
                    self.load_and_tessellate();
                    self.update_projection();
                }
                self.render();
            }

            _ => {}
        }
    }
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,wgpu=warn")),
        )
        .init();

    let args = Args::parse();
    println!("=== Osmic Viewer ===");
    println!("File: {}", args.pmtiles_file.display());
    println!("Controls: drag=pan, scroll=zoom");

    let event_loop = EventLoop::new().unwrap();
    let mut app = MapApp::new(&args);
    event_loop.run_app(&mut app).unwrap();
}
