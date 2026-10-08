//! The text overlay: labels and the info panel, rasterised on the CPU into a
//! premultiplied RGBA image the GPU draws over the map.
//!
//! Placement is [`osmic_text::LabelPlacer`]; the overlay only gathers the
//! labels of the tiles in view, converts them to physical screen pixels and
//! decides *when* a fresh layout is needed. Re-placing and uploading a
//! full-screen image every frame is expensive (tens of megabytes on a
//! high-resolution display), so while the user pans or zooms the previous
//! layout is moved and scaled with the map, and changes to the labels (tiles
//! arriving) are coalesced.

use std::time::{Duration, Instant};

use osmic_core::Color;
use osmic_render::{Camera, TileTransform};
use osmic_text::{Canvas, LabelCandidate, LabelPlacer, Rect, TextEngine};

use crate::info::InfoPanel;
use crate::renderer::OverlayPlacement;

/// The map moved this many physical pixels since the last layout: re-place.
const RELAYOUT_DISTANCE: f64 = 160.0;

/// While zooming, the last layout is scaled instead of re-placed until the
/// zoom differs from it by more than this.
const MAX_SCALED_ZOOM: f64 = 1.0;

/// Label changes (tiles arriving) re-place labels at most this often.
pub const LABEL_REFRESH: Duration = Duration::from_millis(150);

/// Labels of one tile and where the tile is on screen.
pub struct TileLabels<'a> {
    pub transform: TileTransform,
    pub labels: &'a [LabelCandidate],
}

/// Generations of what the overlay shows, bumped by the application.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OverlayContent {
    /// The set of labels changed (tiles loaded or evicted). Coalesced.
    pub labels: u64,
    /// The info panel opened, closed or changed. Shown at once.
    pub panel: u64,
}

/// What the user is doing to the view.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Motion {
    pub dragging: bool,
    /// A zoom gesture is in progress (scroll events are still arriving).
    pub zooming: bool,
}

/// Whether to lay the overlay out again this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// Re-place the labels now.
    Now,
    /// Keep showing the previous layout, moved and scaled with the map.
    Keep,
    /// Keep the previous layout for now, but re-place at this instant.
    KeepUntil(Instant),
}

/// Everything a layout depends on.
#[derive(Debug, Clone, PartialEq)]
struct LayoutKey {
    zoom: f64,
    center: [f64; 2],
    size: [u32; 2],
    content: OverlayContent,
    at: Instant,
}

/// Decides when the overlay must be re-laid-out, and how to place the
/// previous layout meanwhile.
#[derive(Debug, Default)]
pub struct OverlayTracker {
    last: Option<LayoutKey>,
}

impl OverlayTracker {
    /// Whether a new layout is needed for the camera's current state.
    ///
    /// Panning while dragging and zooming within [`MAX_SCALED_ZOOM`] of the
    /// last layout are covered by moving and scaling it
    /// ([`OverlayTracker::placement`]); labels are re-placed once the
    /// gesture ends, after a large movement, or when the panel or the size
    /// changes. Label changes are applied at most every [`LABEL_REFRESH`],
    /// and only once a zoom gesture ends.
    pub fn decide(
        &self,
        camera: &Camera,
        size: [u32; 2],
        content: OverlayContent,
        motion: Motion,
        scale: f64,
        now: Instant,
    ) -> Layout {
        let Some(last) = &self.last else {
            return Layout::Now;
        };
        if last.size != size || last.content.panel != content.panel {
            return Layout::Now;
        }
        let zoom_change = (camera.zoom() - last.zoom).abs();
        if zoom_change > 1e-9 {
            return if motion.zooming && zoom_change <= MAX_SCALED_ZOOM {
                Layout::Keep
            } else {
                Layout::Now
            };
        }
        let moved = self.displacement(camera, scale);
        let distance = moved[0].hypot(moved[1]);
        if distance > RELAYOUT_DISTANCE || (!motion.dragging && distance > 0.0) {
            return Layout::Now;
        }
        if last.content.labels == content.labels || motion.zooming {
            return Layout::Keep;
        }
        let due = last.at + LABEL_REFRESH;
        if now >= due {
            Layout::Now
        } else {
            Layout::KeepUntil(due)
        }
    }

    /// Record that the overlay was laid out for this state at `now`.
    pub fn record(
        &mut self,
        camera: &Camera,
        size: [u32; 2],
        content: OverlayContent,
        now: Instant,
    ) {
        self.last = Some(LayoutKey {
            zoom: camera.zoom(),
            center: camera.center_unit(),
            size,
            content,
            at: now,
        });
    }

    /// Where to draw the last layout so it follows the camera: a world
    /// point at overlay pixel `p` belongs at `offset + p * scale` on screen
    /// (physical pixels). Without a zoom change the offset is rounded to
    /// whole pixels so the text stays crisp.
    pub fn placement(&self, camera: &Camera, scale: f64) -> OverlayPlacement {
        let Some(last) = &self.last else {
            return OverlayPlacement::IDENTITY;
        };
        let d = self.displacement(camera, scale);
        let k = (camera.zoom() - last.zoom).exp2();
        if (k - 1.0).abs() < 1e-9 {
            return OverlayPlacement {
                offset: [d[0].round() as f32, d[1].round() as f32],
                scale: 1.0,
            };
        }
        // Scale about the view center, which the camera keeps fixed.
        let [w, h] = camera.size();
        let half = [w * scale / 2.0, h * scale / 2.0];
        OverlayPlacement {
            offset: [
                (d[0] + half[0] * (1.0 - k)) as f32,
                (d[1] + half[1] * (1.0 - k)) as f32,
            ],
            scale: k as f32,
        }
    }

    /// How far the camera center moved since the last layout, in physical
    /// pixels at the current zoom.
    fn displacement(&self, camera: &Camera, scale: f64) -> [f64; 2] {
        let Some(last) = &self.last else {
            return [0.0, 0.0];
        };
        let world = camera.world_size() * scale;
        let c = camera.center_unit();
        // Shortest way around the world horizontally.
        let mut dx = last.center[0] - c[0];
        dx -= dx.round();
        [dx * world, (last.center[1] - c[1]) * world]
    }
}

/// Inputs to [`render_overlay`].
pub struct OverlayInput<'a> {
    /// Physical pixels per logical pixel.
    pub scale: f64,
    /// Physical size of the overlay.
    pub size: [u32; 2],
    pub tiles: &'a [TileLabels<'a>],
    pub panel: Option<&'a InfoPanel>,
}

/// Place the labels of `input.tiles` and draw them (and the panel) into
/// `pixels`, which becomes a premultiplied RGBA8 image of `input.size`
/// physical pixels. The buffer is reused across layouts, so a full-screen
/// overlay is not reallocated every time.
pub fn render_overlay(engine: &mut TextEngine, input: &OverlayInput<'_>, pixels: &mut Vec<u8>) {
    let [w, h] = input.size;
    pixels.clear();
    pixels.resize(w as usize * h as usize * 4, 0);
    let Some(mut canvas) = Canvas::new(pixels.as_mut_slice(), w, h) else {
        return;
    };
    let scale = input.scale as f32;

    let mut placer = LabelPlacer::new(Rect::new(0.0, 0.0, w as f32, h as f32));
    let panel_layout = input
        .panel
        .map(|p| layout_panel(engine, p, input.scale, [w, h]));
    if let Some(layout) = &panel_layout {
        placer.reserve(layout.rect);
    }

    let mut candidates: Vec<LabelCandidate> = Vec::new();
    for tile in input.tiles {
        let t = tile.transform;
        for label in tile.labels {
            let to_screen = |p: [f32; 2]| {
                [
                    ((t.offset[0] + f64::from(p[0]) * t.scale) as f32) * scale,
                    ((t.offset[1] + f64::from(p[1]) * t.scale) as f32) * scale,
                ]
            };
            candidates.push(LabelCandidate {
                anchor: label.anchor.map(to_screen),
                style: label.style.scaled(scale),
                ..label.clone()
            });
        }
    }
    let placed = placer.place(engine, &candidates);
    engine.draw_labels(&mut canvas, &candidates, &placed);

    if let (Some(panel), Some(layout)) = (input.panel, &panel_layout) {
        draw_panel(engine, &mut canvas, panel, layout, input.scale);
    }
}

struct PanelLayout {
    rect: Rect,
    font: f32,
    padding: f32,
    line_height: f32,
}

fn layout_panel(
    engine: &mut TextEngine,
    panel: &InfoPanel,
    scale: f64,
    size: [u32; 2],
) -> PanelLayout {
    let s = scale as f32;
    let (font, padding) = (13.0 * s, 10.0 * s);
    let line_height = font * 1.45;
    let width = panel
        .lines
        .iter()
        .map(|(_, v)| engine.shape(v, font).width)
        .fold(0.0f32, f32::max)
        + 2.0 * padding;
    let height = panel.lines.len() as f32 * line_height + 2.0 * padding;
    // Next to the click, flipped to the other side if it would leave the view.
    let (view_w, view_h) = (size[0] as f32, size[1] as f32);
    let (ax, ay) = (panel.anchor[0] as f32 * s, panel.anchor[1] as f32 * s);
    let mut x = ax + 12.0 * s;
    if x + width > view_w {
        x = (ax - 12.0 * s - width).max(0.0);
    }
    let mut y = ay;
    if y + height > view_h {
        y = (view_h - height).max(0.0);
    }
    PanelLayout {
        rect: Rect::new(x, y, x + width, y + height),
        font,
        padding,
        line_height,
    }
}

fn draw_panel(
    engine: &mut TextEngine,
    canvas: &mut Canvas<'_>,
    panel: &InfoPanel,
    layout: &PanelLayout,
    scale: f64,
) {
    let r = layout.rect;
    let (x, y) = (r.min[0] as i32, r.min[1] as i32);
    let (w, h) = (r.width().ceil() as u32, r.height().ceil() as u32);
    let border = (1.0 * scale).round().max(1.0) as u32;
    canvas.fill_rect(x, y, w, h, Color::rgba(0.39, 0.56, 0.78, 0.95));
    canvas.fill_rect(
        x + border as i32,
        y + border as i32,
        w.saturating_sub(2 * border),
        h.saturating_sub(2 * border),
        Color::rgba(0.12, 0.12, 0.16, 0.92),
    );
    for (i, (_, value)) in panel.lines.iter().enumerate() {
        let color = if i == 0 {
            Color::WHITE
        } else {
            Color::rgb(0.78, 0.82, 0.86)
        };
        engine.draw_text(
            canvas,
            value,
            r.min[0] + layout.padding,
            r.min[1] + layout.padding + i as f32 * layout.line_height,
            layout.font,
            color,
        );
    }
}

#[cfg(test)]
mod tests {
    use osmic_text::{LabelAnchor, LabelStyle};

    use super::*;

    fn engine() -> TextEngine {
        TextEngine::with_fonts([include_bytes!(
            "../../../crates/osmic-text/tests/fonts/Cantarell-Regular.ttf"
        )
        .to_vec()])
        .expect("bundled font")
    }

    fn camera() -> Camera {
        Camera::new(8.0, 47.0, 10.0, 400.0, 300.0)
    }

    const SIZE: [u32; 2] = [800, 600];
    const IDLE: Motion = Motion {
        dragging: false,
        zooming: false,
    };
    const DRAGGING: Motion = Motion {
        dragging: true,
        zooming: false,
    };
    const ZOOMING: Motion = Motion {
        dragging: false,
        zooming: true,
    };

    fn content(labels: u64, panel: u64) -> OverlayContent {
        OverlayContent { labels, panel }
    }

    #[test]
    fn first_layout_is_always_needed_then_only_on_change() {
        let cam = camera();
        let now = Instant::now();
        let later = now + LABEL_REFRESH;
        let mut t = OverlayTracker::default();
        let decide =
            |t: &OverlayTracker, cam: &Camera, size, c, at| t.decide(cam, size, c, IDLE, 2.0, at);
        assert_eq!(decide(&t, &cam, SIZE, content(1, 1), now), Layout::Now);
        t.record(&cam, SIZE, content(1, 1), now);
        assert_eq!(decide(&t, &cam, SIZE, content(1, 1), later), Layout::Keep);
        assert_eq!(
            decide(&t, &cam, SIZE, content(2, 1), later),
            Layout::Now,
            "labels changed"
        );
        assert_eq!(
            decide(&t, &cam, SIZE, content(1, 2), now),
            Layout::Now,
            "the panel changed: at once"
        );
        assert_eq!(
            decide(&t, &cam, [900, 600], content(1, 1), now),
            Layout::Now,
            "resized"
        );
        let mut zoomed = cam;
        zoomed.set_zoom(10.5);
        assert_eq!(
            decide(&t, &zoomed, SIZE, content(1, 1), now),
            Layout::Now,
            "zoomed"
        );
    }

    #[test]
    fn label_changes_are_coalesced() {
        let cam = camera();
        let t0 = Instant::now();
        let mut t = OverlayTracker::default();
        t.record(&cam, SIZE, content(1, 0), t0);
        // Tiles keep arriving right after a layout: wait, then re-place
        // once for all of them.
        let soon = t0 + LABEL_REFRESH / 3;
        let due = t0 + LABEL_REFRESH;
        assert_eq!(
            t.decide(&cam, SIZE, content(2, 0), IDLE, 2.0, soon),
            Layout::KeepUntil(due)
        );
        assert_eq!(
            t.decide(&cam, SIZE, content(5, 0), DRAGGING, 2.0, soon),
            Layout::KeepUntil(due)
        );
        assert_eq!(
            t.decide(&cam, SIZE, content(5, 0), IDLE, 2.0, due),
            Layout::Now
        );
        // While zooming, label changes wait for the gesture to end.
        assert_eq!(
            t.decide(&cam, SIZE, content(5, 0), ZOOMING, 2.0, due),
            Layout::Keep
        );
    }

    #[test]
    fn small_drags_shift_the_last_layout_and_release_relayouts() {
        let mut cam = camera();
        let now = Instant::now();
        let c = content(1, 0);
        let mut t = OverlayTracker::default();
        t.record(&cam, SIZE, c, now);
        cam.pan_pixels(30.0, -10.0); // logical px; content moves with the pointer
        assert_eq!(t.decide(&cam, SIZE, c, DRAGGING, 2.0, now), Layout::Keep);
        assert_eq!(
            t.placement(&cam, 2.0),
            OverlayPlacement {
                offset: [60.0, -20.0],
                scale: 1.0
            },
            "physical pixels"
        );
        assert_eq!(
            t.decide(&cam, SIZE, c, IDLE, 2.0, now),
            Layout::Now,
            "released"
        );
        cam.pan_pixels(200.0, 0.0);
        assert_eq!(
            t.decide(&cam, SIZE, c, DRAGGING, 2.0, now),
            Layout::Now,
            "moved too far to fake"
        );
        t.record(&cam, SIZE, c, now);
        assert_eq!(t.placement(&cam, 2.0), OverlayPlacement::IDENTITY);
    }

    #[test]
    fn zooming_scales_the_last_layout_until_the_gesture_ends() {
        let mut cam = camera();
        let now = Instant::now();
        let c = content(1, 0);
        let scale = 2.0;
        let mut t = OverlayTracker::default();
        t.record(&cam, SIZE, c, now);
        // Where a few world points were drawn in the last layout.
        let probes = [[10.0, 20.0], [200.0, 150.0], [390.0, 290.0]];
        let world: Vec<(f64, f64)> = probes
            .iter()
            .map(|p| cam.screen_to_lonlat(p[0], p[1]))
            .collect();

        cam.zoom_at([300.0, 100.0], 0.6);
        assert_eq!(t.decide(&cam, SIZE, c, ZOOMING, scale, now), Layout::Keep);
        // The scaled layout puts each point where the camera now draws it.
        let placed = t.placement(&cam, scale);
        assert!((f64::from(placed.scale) - 0.6f64.exp2()).abs() < 1e-6);
        for (p, (lon, lat)) in probes.iter().zip(&world) {
            let want = cam.lonlat_to_screen(*lon, *lat);
            let got = [
                f64::from(placed.offset[0]) + p[0] * scale * f64::from(placed.scale),
                f64::from(placed.offset[1]) + p[1] * scale * f64::from(placed.scale),
            ];
            assert!(
                (got[0] - want[0] * scale).abs() < 0.01 && (got[1] - want[1] * scale).abs() < 0.01,
                "{got:?} vs {want:?}"
            );
        }

        assert_eq!(
            t.decide(&cam, SIZE, c, IDLE, scale, now),
            Layout::Now,
            "the zoom settled"
        );
        cam.zoom_at([300.0, 100.0], 0.6);
        assert_eq!(
            t.decide(&cam, SIZE, c, ZOOMING, scale, now),
            Layout::Now,
            "too far from the last layout to scale"
        );
    }

    fn render(engine: &mut TextEngine, input: &OverlayInput<'_>) -> Vec<u8> {
        let mut pixels = Vec::new();
        render_overlay(engine, input, &mut pixels);
        pixels
    }

    #[test]
    fn the_pixel_buffer_is_reused_and_cleared() {
        let mut eng = engine();
        let labels = [label("Zurich", [100.0, 100.0], 0)];
        let tiles = [TileLabels {
            transform: TileTransform {
                offset: [0.0, 0.0],
                scale: 1.0,
            },
            labels: &labels,
        }];
        let input = |tiles| OverlayInput {
            scale: 1.0,
            size: [400, 300],
            tiles,
            panel: None,
        };
        let mut pixels = Vec::new();
        render_overlay(&mut eng, &input(&tiles), &mut pixels);
        assert!(!ink(&pixels, 400).is_empty());
        let buffer = pixels.as_ptr();
        render_overlay(&mut eng, &input(&[]), &mut pixels);
        assert_eq!(pixels.as_ptr(), buffer, "same allocation");
        assert!(ink(&pixels, 400).is_empty(), "old labels cleared");
    }

    fn label(text: &str, at: [f32; 2], rank: u32) -> LabelCandidate {
        LabelCandidate {
            text: text.into(),
            anchor: LabelAnchor::Point(at),
            style: LabelStyle {
                font_size: 12.0,
                color: Color::BLACK,
                ..LabelStyle::default()
            },
            layer_rank: rank,
            sort_key: 0.0,
        }
    }

    fn ink(pixels: &[u8], w: u32) -> Vec<(u32, u32)> {
        pixels
            .as_chunks::<4>()
            .0
            .iter()
            .enumerate()
            .filter(|(_, p)| p[3] > 0)
            .map(|(i, _)| (i as u32 % w, i as u32 / w))
            .collect()
    }

    #[test]
    fn labels_are_transformed_by_tile_and_display_scale() {
        let mut eng = engine();
        let labels = [label("Zurich", [100.0, 100.0], 0)];
        // Tile origin at logical (50, 20), drawn at 0.5 scale: the label
        // lands at logical (100, 70) = physical (200, 140) at scale 2.
        let tiles = [TileLabels {
            transform: TileTransform {
                offset: [50.0, 20.0],
                scale: 0.5,
            },
            labels: &labels,
        }];
        let pixels = render(
            &mut eng,
            &OverlayInput {
                scale: 2.0,
                size: [800, 600],
                tiles: &tiles,
                panel: None,
            },
        );
        let pts = ink(&pixels, 800);
        assert!(!pts.is_empty());
        let (min_x, max_x) = (
            pts.iter().map(|p| p.0).min().unwrap(),
            pts.iter().map(|p| p.0).max().unwrap(),
        );
        let cx = f64::from(min_x + max_x) / 2.0;
        assert!((cx - 200.0).abs() < 4.0, "{cx}");
        // Text is sized in physical pixels (12 logical * 2 = 24 px tall box).
        let (min_y, max_y) = (
            pts.iter().map(|p| p.1).min().unwrap(),
            pts.iter().map(|p| p.1).max().unwrap(),
        );
        assert!(
            max_y - min_y > 10 && max_y - min_y < 30,
            "{}",
            max_y - min_y
        );
        assert!((f64::from(min_y + max_y) / 2.0 - 140.0).abs() < 6.0);
    }

    #[test]
    fn labels_from_different_tiles_compete_by_rank() {
        let mut eng = engine();
        let important = [label("Important", [100.0, 100.0], 0)];
        let minor = [label("Minor", [105.0, 100.0], 5)];
        let tf = TileTransform {
            offset: [0.0, 0.0],
            scale: 1.0,
        };
        // The minor label's tile comes first, but rank decides.
        let tiles = [
            TileLabels {
                transform: tf,
                labels: &minor,
            },
            TileLabels {
                transform: tf,
                labels: &important,
            },
        ];
        let both = render(
            &mut eng,
            &OverlayInput {
                scale: 1.0,
                size: [400, 300],
                tiles: &tiles,
                panel: None,
            },
        );
        let only_important = render(
            &mut eng,
            &OverlayInput {
                scale: 1.0,
                size: [400, 300],
                tiles: &tiles[1..],
                panel: None,
            },
        );
        assert_eq!(
            both, only_important,
            "the lower-priority label was rejected"
        );
    }

    #[test]
    fn the_overlay_matches_a_resized_viewport() {
        let mut eng = engine();
        for size in [[1, 1], [123, 77], [2560, 1440]] {
            let pixels = render(
                &mut eng,
                &OverlayInput {
                    scale: 1.0,
                    size,
                    tiles: &[],
                    panel: None,
                },
            );
            assert_eq!(pixels.len(), size[0] as usize * size[1] as usize * 4);
        }
    }

    #[test]
    fn the_info_panel_is_drawn_and_blocks_labels() {
        let mut eng = engine();
        let panel = InfoPanel {
            lines: vec![
                ("Name".into(), "Cafe Central".into()),
                ("Type".into(), "amenity / cafe".into()),
            ],
            anchor: [100.0, 100.0],
        };
        let labels = [label("Underneath", [150.0, 110.0], 0)];
        let tf = TileTransform {
            offset: [0.0, 0.0],
            scale: 1.0,
        };
        let tiles = [TileLabels {
            transform: tf,
            labels: &labels,
        }];
        let with_panel = render(
            &mut eng,
            &OverlayInput {
                scale: 1.0,
                size: [400, 300],
                tiles: &tiles,
                panel: Some(&panel),
            },
        );
        let opaque = with_panel
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[3] > 200)
            .count();
        assert!(opaque > 1000, "panel background is drawn ({opaque})");
        let without = render(
            &mut eng,
            &OverlayInput {
                scale: 1.0,
                size: [400, 300],
                tiles: &tiles,
                panel: None,
            },
        );
        assert!(!ink(&without, 400).is_empty());
        // The label under the panel was not placed, so without the panel
        // there is ink the panel version lacks outside the panel itself.
        assert_ne!(with_panel, without);
        assert!(
            with_panel
                .as_chunks::<4>()
                .0
                .iter()
                .all(|p| p[..3].iter().all(|&c| c <= p[3])),
            "premultiplied"
        );
    }

    #[test]
    fn the_panel_stays_inside_the_view() {
        let mut eng = engine();
        let panel = InfoPanel {
            lines: vec![("Name".into(), "A rather long name to force width".into())],
            anchor: [395.0, 295.0],
        };
        let layout = layout_panel(&mut eng, &panel, 1.0, [400, 300]);
        assert!(layout.rect.min[0] >= 0.0 && layout.rect.max[0] <= 400.0 + 0.5);
        assert!(layout.rect.min[1] >= 0.0 && layout.rect.max[1] <= 300.0 + 0.5);
    }
}
