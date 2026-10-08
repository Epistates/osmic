use std::path::PathBuf;
use std::time::Instant;

use clap::Parser;
use tracing::info;

use osmic_core::Color;
use osmic_core::bbox::BBox;
use osmic_core::geometry::Geometry;
use osmic_geo::projection::bbox_to_tile_range;
use osmic_render::backend::{RenderBackend, RenderConfig};
use osmic_render::scene::{LineCap, LineJoin, RenderFeature, RenderLayer, SceneGraph};
use osmic_render::skia::SkiaBackend;
use osmic_tiles::mvt_decode::{self, DecodedFeature};

#[derive(Parser)]
#[command(name = "render-static", about = "Render a map region to a styled PNG")]
struct Args {
    /// Input PMTiles file
    input: PathBuf,

    /// Output PNG file
    output: PathBuf,

    /// Bounding box: min_lon,min_lat,max_lon,max_lat
    #[arg(long, allow_hyphen_values = true)]
    bbox: String,

    /// Zoom level for tile fetching
    #[arg(long, default_value = "12")]
    zoom: u8,

    /// Image width in pixels
    #[arg(long, default_value = "1024")]
    width: u32,

    /// Image height in pixels
    #[arg(long, default_value = "1024")]
    height: u32,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    let render_bbox = parse_bbox(&args.bbox)?;

    println!("=== Osmic - Static Renderer ===");
    println!("Input:  {}", args.input.display());
    println!("Output: {}", args.output.display());
    println!("Size:   {}x{}", args.width, args.height);
    println!("BBox:   {}", render_bbox);

    let start = Instant::now();

    // Load features from PMTiles
    info!("Loading tiles from PMTiles...");
    let features = load_from_pmtiles(&args.input, &render_bbox, args.zoom).await?;
    info!("Loaded {} features from tiles", features.len());

    // Build scene graph
    info!("Building scene graph...");
    let scene = build_scene(
        &features,
        &render_bbox,
        args.width as f32,
        args.height as f32,
    );
    info!(
        "Scene: {} layers, {} total features",
        scene.layers.len(),
        scene.layers.iter().map(|l| l.features.len()).sum::<usize>()
    );

    // Render
    info!("Rendering...");
    let render_start = Instant::now();
    let config = RenderConfig {
        width: args.width,
        height: args.height,
        ..Default::default()
    };
    let mut backend = SkiaBackend::init(&config)?;
    backend.render(&scene)?;
    info!("Rendered in {:.2}s", render_start.elapsed().as_secs_f64());

    // Save PNG
    let png_data = backend.to_png()?;
    std::fs::write(&args.output, &png_data)?;
    println!(
        "\nSaved {} ({} KB) in {:.1}s",
        args.output.display(),
        png_data.len() / 1024,
        start.elapsed().as_secs_f64()
    );

    Ok(())
}

async fn load_from_pmtiles(
    path: &std::path::Path,
    bbox: &BBox,
    zoom: u8,
) -> Result<Vec<DecodedFeature>, Box<dyn std::error::Error>> {
    use pmtiles::{AsyncPmTilesReader, MmapBackend};

    let backend = MmapBackend::try_from(path).await?;
    let reader: AsyncPmTilesReader<MmapBackend> =
        AsyncPmTilesReader::try_from_source(backend).await?;

    let (min_x, min_y, max_x, max_y) = bbox_to_tile_range(bbox, zoom);
    let n = (1u64 << zoom) as f64;
    let total_tiles = ((max_x - min_x + 1) as u64) * ((max_y - min_y + 1) as u64);
    info!(
        zoom,
        tiles = total_tiles,
        "Fetching tiles {}/{}-{}/{}-{}",
        zoom,
        min_x,
        max_x,
        min_y,
        max_y
    );

    let mut all_features = Vec::new();

    for y in min_y..=max_y {
        for x in min_x..=max_x {
            let coord = pmtiles::TileCoord::new(zoom, x, y)?;
            if let Some(data) = reader.get_tile_decompressed(coord).await? {
                let features = mvt_decode::decode_tile(&data, zoom, x, y, n);
                all_features.extend(features);
            }
        }
    }

    Ok(all_features)
}

fn parse_bbox(s: &str) -> Result<BBox, String> {
    let parts: Vec<f64> = s
        .split(',')
        .map(|p| p.trim().parse::<f64>())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("Invalid bbox: {e}"))?;
    if parts.len() != 4 {
        return Err("bbox must be min_lon,min_lat,max_lon,max_lat".into());
    }
    Ok(BBox::new(parts[0], parts[1], parts[2], parts[3]))
}

fn build_scene(features: &[DecodedFeature], bbox: &BBox, width: f32, height: f32) -> SceneGraph {
    let bg = Color::from_hex("#f8f4f0").unwrap();
    let mut scene = SceneGraph::new(bg);

    let lon_scale = width / bbox.width() as f32;
    let lat_scale = height / bbox.height() as f32;
    let scale = lon_scale.min(lat_scale);

    let to_pixel = |lon: f64, lat: f64| -> [f32; 2] {
        let x = ((lon - bbox.min_lon) * scale as f64) as f32;
        let y = ((bbox.max_lat - lat) * scale as f64) as f32;
        [x, y]
    };

    let mut layer_map: std::collections::BTreeMap<i32, RenderLayer> =
        std::collections::BTreeMap::new();

    for feature in features {
        let fb = feature.geometry.bbox();
        if !fb.intersects(bbox) {
            continue;
        }

        let z_order = layer_z_order(&feature.layer);
        let layer = layer_map
            .entry(z_order)
            .or_insert_with(|| RenderLayer::new(z_order));

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

        if is_area
            && let Some(color) = area_color(&feature.layer, feature.class.as_deref())
            && let Some(rings) = geometry_to_fill(&feature.geometry, &to_pixel)
        {
            layer.push(RenderFeature::Fill {
                coords: rings,
                color,
            });
        }

        if is_line
            && let Some((color, w)) = line_style(&feature.layer, feature.class.as_deref())
            && let Some(coords) = geometry_to_stroke(&feature.geometry, &to_pixel)
        {
            layer.push(RenderFeature::Stroke {
                coords,
                color,
                width: w,
                cap: LineCap::Round,
                join: LineJoin::Round,
            });
        }
    }

    for (_, layer) in layer_map {
        scene.add_layer(layer);
    }
    scene
}

fn layer_z_order(layer: &str) -> i32 {
    match layer {
        "landuse" => 10,
        "natural" => 20,
        "leisure" => 30,
        "water" => 40,
        "building" => 50,
        "boundary" => 60,
        "railway" => 70,
        "highway" => 100,
        _ => 150,
    }
}

fn area_color(layer: &str, class: Option<&str>) -> Option<Color> {
    match layer {
        "building" => Color::from_hex("#dfdbd7"),
        "landuse" => match class.unwrap_or("") {
            "forest" => Color::from_hex("#add19e"),
            "grass" | "meadow" => Color::from_hex("#cdebb0"),
            "farmland" => Color::from_hex("#d5e29e"),
            "residential" => Color::from_hex("#e0d6d0"),
            "commercial" => Color::from_hex("#f2dad9"),
            "industrial" => Color::from_hex("#ebdbe8"),
            _ => Color::from_hex("#d5cfc8"),
        },
        "natural" => match class.unwrap_or("") {
            "wood" => Color::from_hex("#add19e"),
            "water" => Color::from_hex("#aad3df"),
            "grassland" => Color::from_hex("#cdebb0"),
            "sand" | "beach" => Color::from_hex("#f5e9c6"),
            _ => None,
        },
        "leisure" => Color::from_hex("#c8facc"),
        "water" => Color::from_hex("#aad3df"),
        _ => None,
    }
}

fn line_style(layer: &str, class: Option<&str>) -> Option<(Color, f32)> {
    match layer {
        "highway" => {
            let (c, w) = match class.unwrap_or("") {
                "motorway" | "motorway_link" => ("#e892a2", 3.0),
                "trunk" | "trunk_link" => ("#f9b29c", 2.5),
                "primary" | "primary_link" => ("#fcd6a4", 2.0),
                "secondary" | "secondary_link" => ("#f7fabf", 1.5),
                "tertiary" | "tertiary_link" => ("#ffffff", 1.2),
                "residential" | "unclassified" => ("#ffffff", 0.8),
                _ => ("#cccccc", 0.5),
            };
            Some((Color::from_hex(c).unwrap(), w))
        }
        "railway" => Some((Color::from_hex("#bfbfbf").unwrap(), 1.0)),
        "boundary" => Some((Color::from_hex("#9e9cab").unwrap(), 1.0)),
        "water" => Some((Color::from_hex("#aad3df").unwrap(), 1.5)),
        _ => None,
    }
}

fn geometry_to_fill(
    geom: &Geometry,
    to_pixel: &dyn Fn(f64, f64) -> [f32; 2],
) -> Option<Vec<Vec<[f32; 2]>>> {
    match geom {
        Geometry::Polygon(poly) => {
            let mut rings = vec![];
            let ext: Vec<[f32; 2]> = poly
                .exterior()
                .coords()
                .map(|c| to_pixel(c.x, c.y))
                .collect();
            if ext.len() >= 3 {
                rings.push(ext);
            }
            for hole in poly.interiors() {
                let h: Vec<[f32; 2]> = hole.coords().map(|c| to_pixel(c.x, c.y)).collect();
                if h.len() >= 3 {
                    rings.push(h);
                }
            }
            if rings.is_empty() { None } else { Some(rings) }
        }
        _ => None,
    }
}

fn geometry_to_stroke(
    geom: &Geometry,
    to_pixel: &dyn Fn(f64, f64) -> [f32; 2],
) -> Option<Vec<[f32; 2]>> {
    match geom {
        Geometry::Line(ls) => {
            let coords: Vec<[f32; 2]> = ls.coords().map(|c| to_pixel(c.x, c.y)).collect();
            if coords.len() >= 2 {
                Some(coords)
            } else {
                None
            }
        }
        _ => None,
    }
}
