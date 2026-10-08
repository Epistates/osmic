//! Interactive map viewer for PMTiles archives.
//!
//! The viewer is a thin shell: the Web Mercator camera, style evaluation,
//! tessellation and label placement live in `osmic-render`, `osmic-style`
//! and `osmic-text`; this crate adds windowing, background tile loading, a
//! tile cache and the wgpu renderer.

mod app;
mod controller;
#[cfg(test)]
mod frame_tests;
mod gpu;
mod info;
mod loader;
mod overlay;
mod plan;
mod renderer;
mod tile_cache;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use osmic_render::Camera;
use osmic_style::Style;
use winit::event_loop::EventLoop;

use crate::app::{App, Config, UserEvent};
use crate::loader::{PmtilesSource, TileLoader};

#[derive(Parser)]
#[command(name = "osmic-viewer", about = "Interactive map viewer")]
struct Args {
    /// PMTiles file to view
    pmtiles_file: PathBuf,

    /// Initial center longitude (default: the archive's center)
    #[arg(long, allow_hyphen_values = true)]
    lon: Option<f64>,

    /// Initial center latitude (default: the archive's center)
    #[arg(long, allow_hyphen_values = true)]
    lat: Option<f64>,

    /// Initial zoom level (MapLibre convention; default: the archive's)
    #[arg(long)]
    zoom: Option<f64>,

    /// MapLibre style JSON to render with (default: the osmic style)
    #[arg(long)]
    style: Option<PathBuf>,
}

/// Fallbacks when neither the command line nor the archive says where to
/// look: the contiguous United States.
const FALLBACK_VIEW: (f64, f64, f64) = (-98.5, 39.8, 4.0);

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,wgpu=warn,naga=warn")),
        )
        .init();

    match run(Args::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("osmic-viewer: error: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Args) -> Result<(), String> {
    let style = match &args.style {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .map_err(|e| format!("reading style {}: {e}", path.display()))?;
            Style::from_json(&text).map_err(|e| format!("style {}: {e}", path.display()))?
        }
        None => osmic_style::default_style(),
    };
    let source = Arc::new(PmtilesSource::open(&args.pmtiles_file)?);
    let max_zoom = source.max_zoom;

    let (lon, lat, zoom) = {
        let (clon, clat, czoom) = source.center.unwrap_or(FALLBACK_VIEW);
        (
            args.lon.unwrap_or(clon),
            args.lat.unwrap_or(clat),
            args.zoom.unwrap_or(czoom),
        )
    };
    println!("=== Osmic Viewer ===");
    println!("File: {}", args.pmtiles_file.display());
    println!("Controls: drag = pan, scroll = zoom at the cursor, click = inspect");

    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .map_err(|e| format!("creating the event loop: {e}"))?;
    let proxy = event_loop.create_proxy();
    let workers = std::thread::available_parallelism()
        .map_or(2, |n| n.get() / 2)
        .clamp(1, 4);
    let loader = TileLoader::spawn(source, Arc::new(style.clone()), workers, move || {
        // The loop may already be gone during shutdown; that is fine.
        let _ = proxy.send_event(UserEvent::TilesReady);
    })
    .map_err(|e| format!("starting tile loader threads: {e}"))?;

    let mut app = App::new(Config {
        style: Arc::new(style),
        camera: Camera::new(lon, lat, zoom, 1280.0, 800.0),
        max_zoom,
        loader,
    });
    event_loop
        .run_app(&mut app)
        .map_err(|e| format!("event loop: {e}"))?;
    match app.failure.take() {
        Some(message) => Err(message),
        None => Ok(()),
    }
}
