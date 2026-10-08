//! Input handling: pointer and wheel events → camera changes.
//!
//! Window-system independent so it can be tested without a window. All
//! positions the controller receives are **physical** pixels (as winit
//! reports them); it converts to logical pixels with the current scale
//! factor, which is what the camera works in.

use osmic_render::Camera;

/// Pointer movement below this many logical pixels still counts as a click.
const CLICK_SLOP: f64 = 5.0;

/// Zoom levels per wheel "line".
const ZOOM_PER_LINE: f64 = 0.25;

/// Pixels the pointer scrolls per wheel line, for touchpads that report
/// pixel deltas.
const PIXELS_PER_LINE: f64 = 40.0;

/// A scroll gesture.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Scroll {
    /// Notched wheel, in lines.
    Lines(f64),
    /// Smooth scrolling (touchpad), in physical pixels.
    Pixels(f64),
}

/// What an input event requires of the application.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Outcome {
    /// The view changed; redraw.
    pub redraw: bool,
    /// The view changed in a way that invalidates any open info panel.
    pub dismiss_panel: bool,
    /// The user clicked (pressed and released without dragging) at this
    /// logical position.
    pub click: Option<[f64; 2]>,
}

/// Camera plus pointer state.
pub struct ViewController {
    camera: Camera,
    scale_factor: f64,
    cursor: [f64; 2],
    dragging: bool,
    drag_distance: f64,
}

impl ViewController {
    /// A controller for `camera` on a display with `scale_factor` physical
    /// pixels per logical pixel.
    pub fn new(camera: Camera, scale_factor: f64) -> Self {
        Self {
            camera,
            scale_factor: sanitize_scale(scale_factor),
            cursor: [0.0, 0.0],
            dragging: false,
            drag_distance: 0.0,
        }
    }

    pub fn camera(&self) -> &Camera {
        &self.camera
    }

    pub fn scale_factor(&self) -> f64 {
        self.scale_factor
    }

    /// Whether a drag is in progress.
    pub fn is_dragging(&self) -> bool {
        self.dragging
    }

    /// The window's physical size changed.
    pub fn resize(&mut self, physical_width: u32, physical_height: u32) {
        self.camera.resize(
            f64::from(physical_width) / self.scale_factor,
            f64::from(physical_height) / self.scale_factor,
        );
    }

    /// The scale factor changed (window moved to another display, or the
    /// user changed the display scaling). The physical size is given too,
    /// because the logical size is derived from both.
    pub fn set_scale_factor(
        &mut self,
        scale_factor: f64,
        physical_width: u32,
        physical_height: u32,
    ) {
        self.scale_factor = sanitize_scale(scale_factor);
        self.resize(physical_width, physical_height);
    }

    /// The pointer moved to a physical position.
    pub fn cursor_moved(&mut self, x: f64, y: f64) -> Outcome {
        let pos = [x / self.scale_factor, y / self.scale_factor];
        let mut out = Outcome::default();
        if self.dragging {
            let (dx, dy) = (pos[0] - self.cursor[0], pos[1] - self.cursor[1]);
            self.drag_distance += dx.hypot(dy);
            if self.drag_distance >= CLICK_SLOP {
                self.camera.pan_pixels(dx, dy);
                out.redraw = true;
                out.dismiss_panel = true;
            }
        }
        self.cursor = pos;
        out
    }

    /// The left button was pressed or released.
    pub fn left_button(&mut self, pressed: bool) -> Outcome {
        let mut out = Outcome::default();
        if pressed {
            self.dragging = true;
            self.drag_distance = 0.0;
        } else if self.dragging {
            self.dragging = false;
            if self.drag_distance < CLICK_SLOP {
                out.click = Some(self.cursor);
            }
            // Releasing ends the drag; the view may need a final refresh.
            out.redraw = true;
        }
        out
    }

    /// The wheel or touchpad scrolled; zooms around the pointer.
    pub fn scroll(&mut self, scroll: Scroll) -> Outcome {
        let lines = match scroll {
            Scroll::Lines(l) => l,
            Scroll::Pixels(p) => p / self.scale_factor / PIXELS_PER_LINE,
        };
        if !lines.is_finite() || lines == 0.0 {
            return Outcome::default();
        }
        self.camera.zoom_at(self.cursor, lines * ZOOM_PER_LINE);
        Outcome {
            redraw: true,
            dismiss_panel: true,
            click: None,
        }
    }
}

fn sanitize_scale(scale: f64) -> f64 {
    if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn controller() -> ViewController {
        let camera = Camera::new(-98.5, 39.8, 10.0, 640.0, 480.0);
        let mut c = ViewController::new(camera, 2.0);
        c.resize(1280, 960);
        c
    }

    #[test]
    fn logical_size_is_physical_over_scale() {
        let c = controller();
        assert_eq!(c.camera().size(), [640.0, 480.0]);
        let mut c = c;
        c.set_scale_factor(1.0, 1280, 960);
        assert_eq!(c.camera().size(), [1280.0, 960.0]);
        c.set_scale_factor(3.0, 1500, 900);
        assert_eq!(c.camera().size(), [500.0, 300.0]);
        // A bogus scale factor falls back instead of dividing by zero.
        c.set_scale_factor(0.0, 100, 100);
        assert_eq!(c.camera().size(), [100.0, 100.0]);
    }

    #[test]
    fn changing_scale_keeps_the_view() {
        let mut c = controller();
        let (center, zoom) = (c.camera().center(), c.camera().zoom());
        c.set_scale_factor(1.0, 1280, 960);
        assert_eq!((c.camera().center(), c.camera().zoom()), (center, zoom));
    }

    #[test]
    fn wheel_zoom_keeps_the_point_under_the_cursor_fixed() {
        let mut c = controller();
        // Physical (900, 300) is logical (450, 150).
        c.cursor_moved(900.0, 300.0);
        let before = c.camera().screen_to_lonlat(450.0, 150.0);
        let out = c.scroll(Scroll::Lines(2.0));
        assert!(out.redraw && out.dismiss_panel);
        assert!((c.camera().zoom() - 10.5).abs() < 1e-12);
        let after = c.camera().screen_to_lonlat(450.0, 150.0);
        assert!((before.0 - after.0).abs() < 1e-9 && (before.1 - after.1).abs() < 1e-9);
        c.scroll(Scroll::Lines(-6.0));
        let after = c.camera().screen_to_lonlat(450.0, 150.0);
        assert!((before.0 - after.0).abs() < 1e-9 && (before.1 - after.1).abs() < 1e-9);
    }

    #[test]
    fn pixel_scrolling_is_scaled_and_degenerate_input_ignored() {
        let mut c = controller();
        c.scroll(Scroll::Pixels(160.0)); // 160 physical px / 2 / 40 = 2 lines
        assert!((c.camera().zoom() - 10.5).abs() < 1e-12);
        let z = c.camera().zoom();
        assert_eq!(c.scroll(Scroll::Lines(0.0)), Outcome::default());
        assert_eq!(c.scroll(Scroll::Lines(f64::NAN)), Outcome::default());
        assert_eq!(c.camera().zoom(), z);
    }

    #[test]
    fn dragging_pans_by_logical_pixels() {
        let mut c = controller();
        let (lon, lat) = c.camera().center();
        let start = c.camera().lonlat_to_screen(lon, lat);
        c.cursor_moved(100.0, 100.0);
        c.left_button(true);
        let out = c.cursor_moved(300.0, 160.0); // +200,+60 physical = +100,+30 logical
        assert!(out.redraw && out.dismiss_panel);
        let end = c.camera().lonlat_to_screen(lon, lat);
        assert!((end[0] - start[0] - 100.0).abs() < 1e-6);
        assert!((end[1] - start[1] - 30.0).abs() < 1e-6);
    }

    #[test]
    fn moving_without_a_button_does_nothing() {
        let mut c = controller();
        let before = *c.camera();
        let out = c.cursor_moved(500.0, 500.0);
        assert_eq!(out, Outcome::default());
        assert_eq!(*c.camera(), before);
    }

    #[test]
    fn press_and_release_in_place_is_a_click() {
        let mut c = controller();
        c.cursor_moved(400.0, 200.0);
        assert_eq!(c.left_button(true), Outcome::default());
        c.cursor_moved(402.0, 201.0); // within the slop
        let out = c.left_button(false);
        assert_eq!(out.click, Some([201.0, 100.5]));
        assert!(!c.is_dragging());
    }

    #[test]
    fn a_drag_is_not_a_click() {
        let mut c = controller();
        c.cursor_moved(400.0, 200.0);
        c.left_button(true);
        c.cursor_moved(500.0, 200.0);
        let out = c.left_button(false);
        assert_eq!(out.click, None);
        assert!(out.redraw);
    }

    #[test]
    fn small_jitter_while_pressed_does_not_pan() {
        let mut c = controller();
        let before = *c.camera();
        c.cursor_moved(400.0, 200.0);
        c.left_button(true);
        c.cursor_moved(404.0, 202.0);
        assert_eq!(*c.camera(), before);
    }

    #[test]
    fn release_without_press_is_ignored() {
        let mut c = controller();
        assert_eq!(c.left_button(false), Outcome::default());
    }
}
