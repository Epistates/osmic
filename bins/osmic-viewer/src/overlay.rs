//! The text overlay: labels and the info panel, rasterised on the CPU into a
//! premultiplied RGBA image the GPU draws over the map.
//!
//! Placement is [`osmic_text::LabelPlacer`]; the overlay only gathers the
//! labels of the tiles in view, converts them to physical screen pixels and
//! decides *when* a fresh layout is needed (so panning does not re-place
//! labels every frame).

use osmic_core::Color;
use osmic_render::{Camera, TileTransform};
use osmic_text::{Canvas, LabelCandidate, LabelPlacer, Rect, TextEngine};

use crate::info::InfoPanel;

/// The map moved this many physical pixels since the last layout: re-place.
const RELAYOUT_DISTANCE: f64 = 160.0;

/// Labels of one tile and where the tile is on screen.
pub struct TileLabels<'a> {
    pub transform: TileTransform,
    pub labels: &'a [LabelCandidate],
}

/// Everything a layout depends on.
#[derive(Debug, Clone, PartialEq)]
struct LayoutKey {
    zoom: f64,
    center: [f64; 2],
    size: [u32; 2],
    /// Bumped by the application when the set of labels changes (tiles
    /// loaded, evicted) or the panel changes.
    epoch: u64,
}

/// Decides when the overlay must be re-laid-out, and how far to shift the
/// previous layout meanwhile.
#[derive(Debug, Default)]
pub struct OverlayTracker {
    last: Option<LayoutKey>,
}

impl OverlayTracker {
    fn key(camera: &Camera, size: [u32; 2], epoch: u64) -> LayoutKey {
        LayoutKey {
            zoom: camera.zoom(),
            center: camera.center_unit(),
            size,
            epoch,
        }
    }

    /// Whether a new layout is needed for the camera's current state.
    ///
    /// While `dragging`, small movements are covered by shifting the last
    /// layout ([`OverlayTracker::shift`]); once the pointer is released, or
    /// after a large movement, labels are re-placed.
    pub fn needs_layout(
        &self,
        camera: &Camera,
        size: [u32; 2],
        epoch: u64,
        dragging: bool,
        scale: f64,
    ) -> bool {
        let Some(last) = &self.last else {
            return true;
        };
        if last.size != size || last.epoch != epoch || (last.zoom - camera.zoom()).abs() > 1e-9 {
            return true;
        }
        let moved = self.displacement(camera, scale);
        let distance = moved[0].hypot(moved[1]);
        distance > RELAYOUT_DISTANCE || (!dragging && distance > 0.0)
    }

    /// Record that the overlay was laid out for this state.
    pub fn record(&mut self, camera: &Camera, size: [u32; 2], epoch: u64) {
        self.last = Some(Self::key(camera, size, epoch));
    }

    /// How far (physical pixels, rounded) the last layout must be moved to
    /// follow the camera.
    pub fn shift(&self, camera: &Camera, scale: f64) -> [i32; 2] {
        let d = self.displacement(camera, scale);
        [d[0].round() as i32, d[1].round() as i32]
    }

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

/// Place the labels of `input.tiles` and draw them (and the panel) into a
/// premultiplied RGBA8 image of `input.size` physical pixels.
pub fn render_overlay(engine: &mut TextEngine, input: &OverlayInput<'_>) -> Vec<u8> {
    let [w, h] = input.size;
    let mut pixels = vec![0u8; w as usize * h as usize * 4];
    let Some(mut canvas) = Canvas::new(&mut pixels, w, h) else {
        return pixels;
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
    pixels
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

    #[test]
    fn first_layout_is_always_needed_then_only_on_change() {
        let cam = camera();
        let mut t = OverlayTracker::default();
        assert!(t.needs_layout(&cam, [800, 600], 1, false, 2.0));
        t.record(&cam, [800, 600], 1);
        assert!(!t.needs_layout(&cam, [800, 600], 1, false, 2.0));
        assert!(
            t.needs_layout(&cam, [800, 600], 2, false, 2.0),
            "labels changed"
        );
        assert!(t.needs_layout(&cam, [900, 600], 1, false, 2.0), "resized");
        let mut zoomed = cam;
        zoomed.set_zoom(10.5);
        assert!(t.needs_layout(&zoomed, [800, 600], 1, false, 2.0), "zoomed");
    }

    #[test]
    fn small_drags_shift_the_last_layout_and_release_relayouts() {
        let mut cam = camera();
        let mut t = OverlayTracker::default();
        t.record(&cam, [800, 600], 1);
        cam.pan_pixels(30.0, -10.0); // logical px; content moves with the pointer
        assert!(!t.needs_layout(&cam, [800, 600], 1, true, 2.0));
        assert_eq!(t.shift(&cam, 2.0), [60, -20], "physical pixels");
        assert!(t.needs_layout(&cam, [800, 600], 1, false, 2.0), "released");
        cam.pan_pixels(200.0, 0.0);
        assert!(
            t.needs_layout(&cam, [800, 600], 1, true, 2.0),
            "moved too far to fake"
        );
        t.record(&cam, [800, 600], 1);
        assert_eq!(t.shift(&cam, 2.0), [0, 0]);
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
        let pixels = render_overlay(
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
        let both = render_overlay(
            &mut eng,
            &OverlayInput {
                scale: 1.0,
                size: [400, 300],
                tiles: &tiles,
                panel: None,
            },
        );
        let only_important = render_overlay(
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
            let pixels = render_overlay(
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
        let with_panel = render_overlay(
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
        let without = render_overlay(
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
