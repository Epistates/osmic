//! Render a map region from a PMTiles archive to a PNG.
//!
//! Everything — the palette, widths, label rules — comes from an
//! [`osmic_style::Style`] (the built-in default, or any supported MapLibre
//! style JSON via `--style`); the scene building and rasterisation are the
//! `osmic-render` library's.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::time::Instant;

use clap::Parser;
use pmtiles::{AsyncPmTilesReader, MmapBackend};
use tracing::{info, warn};

use osmic_core::TileCoord;
use osmic_core::bbox::BBox;
use osmic_render::camera::TILE_SIZE;
use osmic_render::{
    Camera, RenderConfig, SceneBuilder, SceneOptions, SkiaBackend, backend::RenderBackend,
};
use osmic_style::Style;
use osmic_text::TextEngine;
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

    /// Tile zoom to fetch (default: the zoom matching the image scale,
    /// capped at the archive's maximum)
    #[arg(long)]
    zoom: Option<u8>,

    /// Image width in logical pixels
    #[arg(long, default_value = "1024")]
    width: u32,

    /// Image height in logical pixels
    #[arg(long, default_value = "1024")]
    height: u32,

    /// Device pixel ratio (2 renders a 2x image)
    #[arg(long, default_value = "1.0")]
    pixel_ratio: f32,

    /// MapLibre style JSON to render with (default: the osmic style)
    #[arg(long)]
    style: Option<PathBuf>,

    /// Font file(s) to use instead of the system fonts (repeatable)
    #[arg(long)]
    font: Vec<PathBuf>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    if let Err(e) = run(Args::parse()).await {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

async fn run(args: Args) -> Result<(), Box<dyn Error>> {
    let bbox = parse_bbox(&args.bbox)?;
    if !(args.width > 0 && args.height > 0) {
        return Err("width and height must be positive".into());
    }

    let style = match &args.style {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .map_err(|e| format!("reading style {}: {e}", path.display()))?;
            Style::from_json(&text).map_err(|e| format!("style {}: {e}", path.display()))?
        }
        None => osmic_style::default_style(),
    };
    let text = if args.font.is_empty() {
        TextEngine::system()
    } else {
        let fonts = args
            .font
            .iter()
            .map(|p| std::fs::read(p).map_err(|e| format!("reading font {}: {e}", p.display())))
            .collect::<Result<Vec<_>, _>>()?;
        TextEngine::with_fonts(fonts)?
    };

    println!("=== Osmic - Static Renderer ===");
    println!("Input:  {}", args.input.display());
    println!("Output: {}", args.output.display());
    println!(
        "Size:   {}x{} @{}x",
        args.width, args.height, args.pixel_ratio
    );
    println!("BBox:   {bbox}");
    let start = Instant::now();

    let camera = Camera::fit_bbox(&bbox, f64::from(args.width), f64::from(args.height), 0.0);
    let reader = open_archive(&args.input).await?;
    let max_zoom = reader.get_header().max_zoom;
    let tile_zoom = args.zoom.unwrap_or_else(|| camera.tile_zoom(max_zoom));
    info!(map_zoom = camera.zoom(), tile_zoom, "view");

    // One scene per tile, each clipped to its tile so features overlapping
    // into a neighbour's buffer are drawn once; labels are placed together
    // by the backend.
    let builder = SceneBuilder::new(&style);
    let mapping = camera.pixel_mapping();
    let (w, h) = (args.width as f32, args.height as f32);
    let options = |clip| SceneOptions {
        zoom: camera.zoom(),
        mapping,
        cull: Some([-64.0, -64.0, w + 64.0, h + 64.0]),
        clip,
    };
    let mut scene = builder.build(&[], &options(None));
    let (mut tiles, mut features_total) = (0usize, 0usize);
    for visible in camera.visible_tiles(tile_zoom, 0.0) {
        if visible.world != 0 {
            continue; // static maps do not wrap around the antimeridian
        }
        let Some(features) = load_tile(&reader, visible.coord).await? else {
            continue;
        };
        let t = camera.tile_transform(visible.coord, 0);
        let size = TILE_SIZE * t.scale;
        let clip = [
            t.offset[0] as f32,
            t.offset[1] as f32,
            (t.offset[0] + size) as f32,
            (t.offset[1] + size) as f32,
        ];
        scene.append(builder.build(&features, &options(Some(clip))));
        tiles += 1;
        features_total += features.len();
    }
    info!(
        tiles,
        features = features_total,
        primitives = scene.feature_count(),
        "scene built"
    );

    let render_start = Instant::now();
    let config = RenderConfig {
        width: args.width,
        height: args.height,
        pixel_ratio: args.pixel_ratio,
        ..RenderConfig::default()
    };
    let mut backend = SkiaBackend::with_text_engine(&config, text)?;
    backend.render(&scene)?;
    info!("rendered in {:.2}s", render_start.elapsed().as_secs_f64());

    let png = backend.to_png()?;
    std::fs::write(&args.output, &png)
        .map_err(|e| format!("writing {}: {e}", args.output.display()))?;
    println!(
        "\nSaved {} ({} KB) in {:.1}s",
        args.output.display(),
        png.len() / 1024,
        start.elapsed().as_secs_f64()
    );
    Ok(())
}

async fn open_archive(path: &Path) -> Result<AsyncPmTilesReader<MmapBackend>, Box<dyn Error>> {
    let backend = MmapBackend::try_from(path)
        .await
        .map_err(|e| format!("opening {}: {e}", path.display()))?;
    Ok(AsyncPmTilesReader::try_from_source(backend)
        .await
        .map_err(|e| format!("reading PMTiles header of {}: {e}", path.display()))?)
}

/// Decode one tile; `None` if the archive has no such tile or it is
/// undecodable (logged).
async fn load_tile(
    reader: &AsyncPmTilesReader<MmapBackend>,
    tile: TileCoord,
) -> Result<Option<Vec<DecodedFeature>>, Box<dyn Error>> {
    let coord = pmtiles::TileCoord::new(tile.z.0, tile.x, tile.y)?;
    let Some(data) = reader.get_tile_decompressed(coord).await? else {
        return Ok(None);
    };
    match mvt_decode::decode_tile(&data, tile) {
        Ok(features) => Ok(Some(features)),
        Err(e) => {
            warn!(%tile, error = %e, "skipping undecodable tile");
            Ok(None)
        }
    }
}

fn parse_bbox(s: &str) -> Result<BBox, String> {
    let parts: Vec<f64> = s
        .split(',')
        .map(|p| p.trim().parse::<f64>())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("invalid bbox: {e}"))?;
    let [min_lon, min_lat, max_lon, max_lat] = parts[..] else {
        return Err("bbox must be min_lon,min_lat,max_lon,max_lat".into());
    };
    if !(min_lon < max_lon && min_lat < max_lat) {
        return Err("bbox minimums must be below its maximums".into());
    }
    Ok(BBox::new(min_lon, min_lat, max_lon, max_lat))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bbox_parsing() {
        let b = parse_bbox("-122.52, 37.70, -122.35, 37.82").unwrap();
        assert_eq!((b.min_lon, b.max_lat), (-122.52, 37.82));
        assert!(parse_bbox("1,2,3").is_err());
        assert!(parse_bbox("a,b,c,d").is_err());
        assert!(parse_bbox("5,5,1,1").is_err(), "inverted box");
    }

    /// A one-tile archive (z8/70/95): a forest polygon, a named road and a
    /// named shop.
    fn write_archive(path: &Path) {
        use osmic_tiles::assemble::TileCompression;
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
                    vec![vec![[0, 0], [4096, 0], [4096, 4096], [0, 4096]]],
                    &[("class", "forest")],
                )],
            ),
            layer(
                "highway",
                vec![feature(
                    GeomType::LineString,
                    vec![vec![[0, 2000], [4096, 2000]]],
                    &[("class", "primary"), ("name", "Main Street")],
                )],
            ),
        ]);
        let coord = TileCoord::new(70, 95, osmic_core::Zoom(8));
        let mut archive = PmTilesArchive::create(
            path,
            &ArchiveOptions {
                format: TileFormat::Mvt,
                compression: TileCompression::None,
                bounds: coord.bbox(),
                min_zoom: 8,
                max_zoom: 8,
                metadata: serde_json::json!({}),
                overwrite: true,
            },
        )
        .unwrap();
        archive.add_tile(coord, &tile).unwrap();
        archive.finalize().unwrap();
    }

    fn args(dir: &Path, bbox: &str) -> Args {
        Args {
            input: dir.join("tiny.pmtiles"),
            output: dir.join("out.png"),
            bbox: bbox.into(),
            zoom: None,
            width: 256,
            height: 192,
            pixel_ratio: 1.0,
            style: None,
            font: vec![PathBuf::from(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../crates/osmic-text/tests/fonts/Cantarell-Regular.ttf"
            ))],
        }
    }

    fn png_size(path: &Path) -> (u32, u32) {
        let bytes = std::fs::read(path).unwrap();
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
        let be = |i: usize| u32::from_be_bytes(bytes[i..i + 4].try_into().unwrap());
        (be(16), be(20))
    }

    #[tokio::test]
    async fn renders_an_archive_to_a_png_with_the_library_pipeline() {
        let dir = tempfile::tempdir().unwrap();
        write_archive(&dir.path().join("tiny.pmtiles"));
        let bb = TileCoord::new(70, 95, osmic_core::Zoom(8)).bbox();
        let bbox = format!(
            "{},{},{},{}",
            bb.min_lon, bb.min_lat, bb.max_lon, bb.max_lat
        );

        run(args(dir.path(), &bbox)).await.unwrap();
        assert_eq!(png_size(&dir.path().join("out.png")), (256, 192));

        let mut hidpi = args(dir.path(), &bbox);
        hidpi.pixel_ratio = 2.0;
        run(hidpi).await.unwrap();
        assert_eq!(png_size(&dir.path().join("out.png")), (512, 384));
    }

    #[tokio::test]
    async fn bad_inputs_are_reported_as_errors() {
        let dir = tempfile::tempdir().unwrap();
        // Missing archive.
        let err = run(args(dir.path(), "0,0,1,1"))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("tiny.pmtiles"), "{err}");
        write_archive(&dir.path().join("tiny.pmtiles"));
        // Bad bbox, bad style, bad font.
        assert!(run(args(dir.path(), "nonsense")).await.is_err());
        let mut a = args(dir.path(), "0,0,1,1");
        let style = dir.path().join("style.json");
        std::fs::write(&style, r#"{"version": 8, "sources": {}, "layers": [{"id": "x", "type": "raster", "source": "s"}]}"#).unwrap();
        a.style = Some(style);
        let err = run(a).await.unwrap_err().to_string();
        assert!(err.contains("raster"), "{err}");
        let mut a = args(dir.path(), "0,0,1,1");
        a.font = vec![dir.path().join("missing.ttf")];
        assert!(run(a).await.is_err());
    }
}
