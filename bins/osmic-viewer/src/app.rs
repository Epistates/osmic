//! The windowed application: wires input, tile loading, the cache, the
//! label overlay and the GPU renderer together.

use std::sync::Arc;

use osmic_core::{Color, TileCoord};
use osmic_render::Camera;
use osmic_style::{EvalContext, LayerKind, Style};
use osmic_text::{LabelCandidate, TextEngine};
use tracing::{debug, info, warn};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::window::{WindowAttributes, WindowId};

use crate::controller::{Outcome, Scroll, ViewController};
use crate::gpu::{FrameOutcome, Gpu, region_scissor};
use crate::info::{self, InfoPanel};
use crate::loader::{LoadedTile, Poi, TileLoader};
use crate::overlay::{OverlayInput, OverlayTracker, TileLabels, render_overlay};
use crate::plan::plan_draws;
use crate::renderer::{GpuTile, TileDraw, tile_draw_uniform};
use crate::tile_cache::{TileCache, Weigh};

/// Events sent to the event loop from other threads.
#[derive(Debug, Clone, Copy)]
pub enum UserEvent {
    /// A background worker finished a tile.
    TilesReady,
}

/// Tiles requested beyond the screen edge, in logical pixels, so panning
/// finds data already loaded.
const PREFETCH_MARGIN: f64 = 256.0;

/// Cache limits: tiles and bytes of mesh data.
const CACHE_ENTRIES: usize = 600;
const CACHE_BYTES: usize = 512 * 1024 * 1024;

/// A tile as the render loop holds it.
struct CachedTile {
    /// `None` for tiles with nothing to draw (absent, empty or failed).
    gpu: Option<GpuTile>,
    labels: Vec<LabelCandidate>,
    pois: Vec<Poi>,
    weight: usize,
}

impl Weigh for CachedTile {
    fn weight(&self) -> usize {
        self.weight
    }
}

/// Everything the application needs that is not window-system state.
pub struct Config {
    pub style: Arc<Style>,
    pub camera: Camera,
    pub max_zoom: u8,
    pub loader: TileLoader,
}

pub struct App {
    style: Arc<Style>,
    max_zoom: u8,
    loader: TileLoader,
    cache: TileCache<CachedTile>,
    /// Tiles the last frame drew (sources of its plan).
    drawn: Vec<TileCoord>,
    controller: ViewController,
    gpu: Option<Gpu>,
    text: TextEngine,
    tracker: OverlayTracker,
    /// Bumped whenever the overlay's contents (loaded tiles, panel) change.
    epoch: u64,
    panel: Option<InfoPanel>,
    /// Set when the viewer must stop with an error.
    pub failure: Option<String>,
}

impl App {
    pub fn new(config: Config) -> Self {
        Self {
            style: config.style,
            max_zoom: config.max_zoom,
            loader: config.loader,
            cache: TileCache::new(CACHE_ENTRIES, CACHE_BYTES),
            drawn: Vec::new(),
            controller: ViewController::new(config.camera, 1.0),
            gpu: None,
            text: TextEngine::system(),
            tracker: OverlayTracker::default(),
            epoch: 0,
            panel: None,
            failure: None,
        }
    }

    /// Stop the event loop, remembering why.
    fn fail(&mut self, event_loop: &ActiveEventLoop, message: String) {
        self.failure.get_or_insert(message);
        event_loop.exit();
    }

    fn request_redraw(&self) {
        if let Some(gpu) = &self.gpu {
            gpu.window().request_redraw();
        }
    }

    fn apply(&mut self, outcome: Outcome) {
        if outcome.dismiss_panel && self.panel.take().is_some() {
            self.epoch += 1;
        }
        if let Some(click) = outcome.click {
            self.click(click);
        }
        if outcome.redraw {
            self.request_redraw();
        }
    }

    fn click(&mut self, click: [f64; 2]) {
        let camera = *self.controller.camera();
        let tile_zoom = camera.tile_zoom(self.max_zoom);
        let visible = camera.visible_tiles(tile_zoom, 0.0);
        // `peek`: picking must not disturb the LRU order.
        let pois = visible
            .iter()
            .filter_map(|v| self.cache.peek(&v.coord))
            .flat_map(|t| t.pois.iter());
        let panel = info::pick(&camera, pois, click).map(|poi| InfoPanel {
            lines: info::describe(poi),
            anchor: click,
        });
        if panel != self.panel {
            self.panel = panel;
            self.epoch += 1;
            self.request_redraw();
        }
    }

    /// Move finished tiles into the cache (uploading them to the GPU).
    /// Returns whether anything arrived.
    fn drain_loader(&mut self) -> bool {
        let mut any = false;
        while let Some(LoadedTile { coord, result }) = self.loader.try_recv() {
            any = true;
            let entry = match result {
                Ok(Some(data)) => {
                    let gpu = self
                        .gpu
                        .as_ref()
                        .and_then(|g| g.renderer().upload_tile(&data.mesh));
                    let weight = data.weight();
                    CachedTile {
                        gpu,
                        labels: data.labels,
                        pois: data.pois,
                        weight,
                    }
                }
                Ok(None) => empty_tile(),
                Err(e) => {
                    warn!(%coord, "tile unavailable: {e}");
                    empty_tile()
                }
            };
            // Evicted tiles drop their GPU buffers here.
            let evicted = self.cache.insert(coord, entry);
            if !evicted.is_empty() {
                debug!(
                    evicted = evicted.len(),
                    cached = self.cache.len(),
                    "tile cache evicted"
                );
            }
        }
        any
    }

    fn redraw(&mut self, event_loop: &ActiveEventLoop) {
        if self.gpu.is_none() {
            return;
        }
        let camera = *self.controller.camera();
        let scale = self.controller.scale_factor();
        let tile_zoom = camera.tile_zoom(self.max_zoom);
        let visible = camera.visible_tiles(tile_zoom, 0.0);

        // Arrivals must not evict what is on screen (or standing in for
        // it): a view needing more than the budget would reload its own
        // tiles over and over.
        self.cache.set_pinned(
            visible
                .iter()
                .map(|v| v.coord)
                .chain(self.drawn.iter().copied()),
        );
        if self.drain_loader() {
            self.epoch += 1;
        }

        // Ask for what is missing, nearest first, including a margin.
        let wanted: Vec<TileCoord> = camera
            .visible_tiles(tile_zoom, PREFETCH_MARGIN)
            .into_iter()
            .map(|v| v.coord)
            .filter(|c| !self.cache.contains(c))
            .collect();
        self.loader.request(&wanted);

        let plan = plan_draws(&camera, &visible, |c| self.cache.contains(c));
        // Mark everything in use as recently used.
        self.drawn.clear();
        for item in &plan {
            self.cache.get(&item.source);
            self.drawn.push(item.source);
        }

        let Some(gpu) = self.gpu.as_mut() else { return };
        let size = gpu.size();

        // Labels: re-place when the view or the tiles changed enough,
        // otherwise slide the previous layout along with the map.
        if self.tracker.needs_layout(
            &camera,
            size,
            self.epoch,
            self.controller.is_dragging(),
            scale,
        ) {
            // A stand-in tile may serve several regions; its labels count once.
            let mut seen: Vec<TileCoord> = Vec::new();
            let mut tiles: Vec<TileLabels<'_>> = Vec::new();
            for item in &plan {
                if seen.contains(&item.source) {
                    continue;
                }
                seen.push(item.source);
                if let Some(t) = self.cache.peek(&item.source) {
                    tiles.push(TileLabels {
                        transform: item.transform,
                        labels: &t.labels,
                    });
                }
            }
            let rgba = render_overlay(
                &mut self.text,
                &OverlayInput {
                    scale,
                    size,
                    tiles: &tiles,
                    panel: self.panel.as_ref(),
                },
            );
            gpu.renderer_mut().upload_overlay(&rgba, size);
            self.tracker.record(&camera, size, self.epoch);
        }
        gpu.renderer_mut()
            .set_overlay_shift(self.tracker.shift(&camera, scale));

        let logical = camera.size();
        let draws: Vec<TileDraw<'_>> = plan
            .iter()
            .filter_map(|item| {
                let tile = self.cache.peek(&item.source)?.gpu.as_ref()?;
                Some(TileDraw {
                    tile,
                    uniform: tile_draw_uniform(
                        item.transform,
                        item.source.z.0,
                        camera.zoom(),
                        logical,
                        gpu.renderer().linearize(),
                    ),
                    scissor: region_scissor(item.region, scale, size)?,
                })
            })
            .collect();

        match gpu.render(background_color(&self.style, camera.zoom()), &draws) {
            FrameOutcome::Presented | FrameOutcome::Skipped => {}
            FrameOutcome::Reconfigured => gpu.window().request_redraw(),
            FrameOutcome::Lost => {
                self.fail(event_loop, "the window surface was lost".to_string());
            }
        }
    }
}

fn empty_tile() -> CachedTile {
    CachedTile {
        gpu: None,
        labels: Vec::new(),
        pois: Vec::new(),
        weight: 64,
    }
}

/// The color the style's background layer gives at `zoom` (white if the
/// style has none).
pub fn background_color(style: &Style, zoom: f64) -> Color {
    style
        .layers
        .iter()
        .find_map(|l| match &l.kind {
            LayerKind::Background(bg) if l.is_active_at(zoom) => {
                Some(bg.resolve(&EvalContext::at_zoom(zoom)))
            }
            _ => None,
        })
        .unwrap_or(Color::WHITE)
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.gpu.is_some() {
            return;
        }
        let window = match event_loop.create_window(
            WindowAttributes::default()
                .with_title("Osmic Viewer")
                .with_inner_size(LogicalSize::new(1280.0, 800.0)),
        ) {
            Ok(w) => Arc::new(w),
            Err(e) => return self.fail(event_loop, format!("creating the window: {e}")),
        };
        let size = window.inner_size();
        let scale = window.scale_factor();
        let gpu = match Gpu::new(Arc::clone(&window)) {
            Ok(g) => g,
            Err(e) => return self.fail(event_loop, e),
        };
        info!(
            width = size.width,
            height = size.height,
            scale,
            "window ready"
        );
        self.controller
            .set_scale_factor(scale, size.width, size.height);
        self.gpu = Some(gpu);
        self.request_redraw();
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::TilesReady => self.request_redraw(),
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),

            WindowEvent::Resized(size) => {
                if let Some(gpu) = &mut self.gpu {
                    gpu.resize(size.width, size.height);
                }
                self.controller.resize(size.width, size.height);
                self.request_redraw();
            }

            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                // The size that goes with the new factor follows in a
                // `Resized`; use what the window reports now so the logical
                // size is consistent in the meantime.
                if let Some(gpu) = &self.gpu {
                    let size = gpu.window().inner_size();
                    self.controller
                        .set_scale_factor(scale_factor, size.width, size.height);
                }
                self.request_redraw();
            }

            WindowEvent::CursorMoved { position, .. } => {
                let outcome = self.controller.cursor_moved(position.x, position.y);
                self.apply(outcome);
            }

            WindowEvent::MouseInput {
                state,
                button: MouseButton::Left,
                ..
            } => {
                let outcome = self.controller.left_button(state == ElementState::Pressed);
                self.apply(outcome);
            }

            WindowEvent::MouseWheel { delta, .. } => {
                let scroll = match delta {
                    MouseScrollDelta::LineDelta(_, y) => Scroll::Lines(f64::from(y)),
                    MouseScrollDelta::PixelDelta(p) => Scroll::Pixels(p.y),
                };
                let outcome = self.controller.scroll(scroll);
                self.apply(outcome);
            }

            WindowEvent::RedrawRequested => self.redraw(event_loop),

            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use osmic_style::default_style;

    use super::*;

    #[test]
    fn background_comes_from_the_style() {
        let style = default_style();
        assert_eq!(background_color(&style, 3.0).to_css(), "#f8f4f0");
        assert_eq!(background_color(&Style::new("empty"), 3.0), Color::WHITE);
    }
}
