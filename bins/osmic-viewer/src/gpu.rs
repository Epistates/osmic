//! Window and surface glue around the [`Renderer`]: adapter and device
//! selection, surface configuration, and recovery from surface errors.
//!
//! The surface format is chosen non-sRGB whenever the platform offers one
//! (see [`crate::renderer`] for why), and 4x MSAA is used when supported.

use std::sync::Arc;

use osmic_core::Color;
use tracing::{debug, error, warn};
use winit::window::Window;

use crate::plan::scissor;
use crate::renderer::{Renderer, TileDraw};

/// The sample count to use for MSAA.
pub const MSAA_SAMPLES: u32 = 4;

/// How a frame ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameOutcome {
    Presented,
    /// Nothing can be drawn until something changes (the window is occluded
    /// or minimised, or acquiring the frame failed validation): wait for
    /// the next window event rather than retrying in a loop.
    Skipped,
    /// Acquiring the frame timed out; draw again. (The acquire itself
    /// waited for the timeout, so retrying does not spin.)
    Retry,
    /// The surface was stale or lost and has been reconfigured or
    /// recreated; draw again.
    Reconfigured,
    /// The surface is gone and recreating it keeps failing; the
    /// application should exit.
    Lost,
}

/// Consecutive lost surfaces tolerated (each one recreated) before giving
/// up.
const MAX_SURFACE_LOSSES: u32 = 3;

/// Counts surface losses since the last presented frame.
#[derive(Debug, Default)]
struct LossBudget {
    losses: u32,
}

impl LossBudget {
    /// A loss happened: whether recreating the surface is still worth it.
    fn lost(&mut self) -> bool {
        self.losses += 1;
        self.losses <= MAX_SURFACE_LOSSES
    }

    fn presented(&mut self) {
        self.losses = 0;
    }
}

/// Prefer a non-sRGB surface format; otherwise take the first one and
/// report that colors need linearising. Returns `None` for an empty list.
pub fn choose_surface_format(
    formats: &[wgpu::TextureFormat],
) -> Option<(wgpu::TextureFormat, bool)> {
    formats
        .iter()
        .copied()
        .find(|f| !f.is_srgb())
        .map(|f| (f, false))
        .or_else(|| formats.first().map(|f| (*f, true)))
}

/// 4x MSAA when the format supports it, otherwise none.
pub fn choose_sample_count(supports: impl Fn(u32) -> bool) -> u32 {
    if supports(MSAA_SAMPLES) {
        MSAA_SAMPLES
    } else {
        1
    }
}

/// Physical scissor for a logical region.
pub fn region_scissor(region: [f64; 4], scale: f64, size: [u32; 2]) -> Option<[u32; 4]> {
    scissor(region, scale, size[0], size[1])
}

/// A window's surface and the renderer drawing into it.
pub struct Gpu {
    window: Arc<Window>,
    /// Kept to recreate the surface when it is lost.
    instance: wgpu::Instance,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    max_texture: u32,
    losses: LossBudget,
    renderer: Renderer,
}

impl Gpu {
    /// Create the surface, device and pipelines for `window`.
    pub fn new(window: Arc<Window>) -> Result<Self, String> {
        let size = window.inner_size();
        let instance = wgpu::Instance::default();
        let surface = instance
            .create_surface(Arc::clone(&window))
            .map_err(|e| format!("creating the window surface: {e}"))?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            compatible_surface: Some(&surface),
            ..Default::default()
        }))
        .map_err(|e| format!("no suitable graphics adapter: {e}"))?;
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("osmic-viewer device"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default().using_resolution(adapter.limits()),
            ..Default::default()
        }))
        .map_err(|e| format!("creating the graphics device: {e}"))?;
        // Validation errors (for example from a transient bad frame) are
        // logged rather than aborting the viewer.
        device.on_uncaptured_error(Arc::new(|e| error!("wgpu error: {e}")));

        let caps = surface.get_capabilities(&adapter);
        let (format, linearize) = choose_surface_format(&caps.formats)
            .ok_or_else(|| "the surface reports no supported texture formats".to_string())?;
        let max_texture = device.limits().max_texture_dimension_2d;
        let mut config = surface
            .get_default_config(&adapter, size.width.max(1), size.height.max(1))
            .ok_or_else(|| "the surface is not supported by the adapter".to_string())?;
        config.format = format;
        config.width = config.width.clamp(1, max_texture);
        config.height = config.height.clamp(1, max_texture);
        surface.configure(&device, &config);

        let samples = choose_sample_count(|n| {
            adapter
                .get_texture_format_features(format)
                .flags
                .sample_count_supported(n)
        });
        debug!(?format, linearize, samples, "surface configured");

        let renderer = Renderer::new(
            device,
            queue,
            format,
            linearize,
            samples,
            [config.width, config.height],
        );
        Ok(Self {
            window,
            instance,
            surface,
            config,
            max_texture,
            losses: LossBudget::default(),
            renderer,
        })
    }

    pub fn window(&self) -> &Arc<Window> {
        &self.window
    }

    pub fn renderer(&self) -> &Renderer {
        &self.renderer
    }

    pub fn renderer_mut(&mut self) -> &mut Renderer {
        &mut self.renderer
    }

    /// Physical size of the surface.
    pub fn size(&self) -> [u32; 2] {
        [self.config.width, self.config.height]
    }

    /// Reconfigure for a new physical size.
    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return; // minimised
        }
        let size = [width.min(self.max_texture), height.min(self.max_texture)];
        if size == self.size() {
            return;
        }
        self.config.width = size[0];
        self.config.height = size[1];
        self.surface.configure(self.renderer.device(), &self.config);
        self.renderer.resize(size);
    }

    /// Reconfigure the surface for the window's current size, which may
    /// have changed without a resize event reaching us yet. `false` while
    /// the window has no area (minimised).
    fn reconfigure(&mut self) -> bool {
        let size = self.window.inner_size();
        if size.width == 0 || size.height == 0 {
            return false;
        }
        let size = [
            size.width.min(self.max_texture),
            size.height.min(self.max_texture),
        ];
        self.config.width = size[0];
        self.config.height = size[1];
        self.surface.configure(self.renderer.device(), &self.config);
        self.renderer.resize(size);
        true
    }

    /// Replace a lost surface with a new one for the same window.
    fn recreate_surface(&mut self) -> Result<(), wgpu::CreateSurfaceError> {
        self.surface = self.instance.create_surface(Arc::clone(&self.window))?;
        Ok(())
    }

    /// Draw one frame.
    pub fn render(&mut self, clear: Color, draws: &[TileDraw<'_>]) -> FrameOutcome {
        use wgpu::CurrentSurfaceTexture as Acquired;
        let (frame, suboptimal) = match self.surface.get_current_texture() {
            Acquired::Success(frame) => (frame, false),
            Acquired::Suboptimal(frame) => (frame, true),
            Acquired::Outdated => {
                return if self.reconfigure() {
                    FrameOutcome::Reconfigured
                } else {
                    FrameOutcome::Skipped
                };
            }
            Acquired::Lost => {
                if !self.losses.lost() {
                    error!("the window surface keeps getting lost; giving up");
                    return FrameOutcome::Lost;
                }
                warn!("the window surface was lost; recreating it");
                if let Err(e) = self.recreate_surface() {
                    error!("recreating the window surface: {e}");
                    return FrameOutcome::Lost;
                }
                return if self.reconfigure() {
                    FrameOutcome::Reconfigured
                } else {
                    FrameOutcome::Skipped
                };
            }
            Acquired::Timeout => {
                debug!("acquiring the frame timed out; retrying");
                return FrameOutcome::Retry;
            }
            status @ (Acquired::Occluded | Acquired::Validation) => {
                debug!("skipping frame: {status:?}");
                return FrameOutcome::Skipped;
            }
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        self.renderer.draw(&view, clear, draws);
        self.renderer.queue().present(frame);
        self.losses.presented();
        if suboptimal {
            self.reconfigure();
        }
        FrameOutcome::Presented
    }
}

#[cfg(test)]
mod tests {
    use wgpu::TextureFormat as F;

    use super::*;

    #[test]
    fn prefers_a_non_srgb_surface() {
        assert_eq!(
            choose_surface_format(&[F::Bgra8UnormSrgb, F::Bgra8Unorm]),
            Some((F::Bgra8Unorm, false))
        );
        assert_eq!(
            choose_surface_format(&[F::Rgba8Unorm, F::Rgba8UnormSrgb]),
            Some((F::Rgba8Unorm, false))
        );
    }

    #[test]
    fn falls_back_to_srgb_with_linearisation() {
        assert_eq!(
            choose_surface_format(&[F::Bgra8UnormSrgb]),
            Some((F::Bgra8UnormSrgb, true))
        );
        assert_eq!(choose_surface_format(&[]), None);
    }

    #[test]
    fn lost_surfaces_are_recreated_until_losses_keep_repeating() {
        let mut budget = LossBudget::default();
        for _ in 0..MAX_SURFACE_LOSSES {
            assert!(budget.lost(), "recreate");
        }
        assert!(!budget.lost(), "lost again without a frame in between");
        budget.presented();
        assert!(budget.lost(), "a presented frame resets the count");
    }

    #[test]
    fn msaa_is_used_only_when_supported() {
        assert_eq!(choose_sample_count(|n| n == 4), 4);
        assert_eq!(choose_sample_count(|_| false), 1);
    }
}
