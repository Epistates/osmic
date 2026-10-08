//! Coverage masks and the one premultiplied-alpha compositing routine.
//!
//! Every pixel buffer in osmic that receives text — the software renderer's
//! pixmap and the viewer's label overlay — is **premultiplied RGBA8**, and
//! every glyph is composited through [`composite_mask`] /
//! [`blend_over`]. There is no other blending code.

use osmic_core::Color;

/// An 8-bit coverage mask (0 = outside, 255 = fully covered).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mask {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

impl Mask {
    /// An empty mask.
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            data: vec![0; width as usize * height as usize],
        }
    }

    /// Coverage at `(x, y)`; zero outside the mask.
    #[inline]
    pub fn get(&self, x: i32, y: i32) -> u8 {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            0
        } else {
            self.data[y as usize * self.width as usize + x as usize]
        }
    }

    /// Accumulate `coverage` at `(x, y)` with source-over semantics
    /// (`a + b - ab`), ignoring out-of-range coordinates.
    #[inline]
    pub fn add(&mut self, x: i32, y: i32, coverage: u8) {
        if coverage == 0 || x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return;
        }
        let px = &mut self.data[y as usize * self.width as usize + x as usize];
        *px = (u16::from(*px) + u16::from(coverage) - u16::from(mul255(*px, coverage))) as u8;
    }

    /// Whether no pixel is covered.
    pub fn is_empty(&self) -> bool {
        self.data.iter().all(|&c| c == 0)
    }

    /// Morphological dilation by a disc of `radius` pixels: each output
    /// pixel is the maximum coverage within `radius` of it. This produces
    /// the halo around text in one pass instead of drawing offset copies.
    ///
    /// The mask keeps its size, so callers should allocate at least
    /// `radius.ceil()` pixels of padding around the glyphs.
    pub fn dilate(&self, radius: f32) -> Mask {
        if radius <= 0.0 || self.width == 0 || self.height == 0 {
            return self.clone();
        }
        let r = radius.ceil() as i32;
        // Half-width of the disc on each row, with half a pixel of slack so
        // small radii still grow the shape.
        let reach = |dy: i32| -> i32 {
            let rr = (radius + 0.5) * (radius + 0.5) - (dy * dy) as f32;
            if rr < 0.0 {
                -1
            } else {
                rr.sqrt().floor() as i32
            }
        };
        let spans: Vec<(i32, i32)> = (-r..=r)
            .map(|dy| (dy, reach(dy)))
            .filter(|(_, h)| *h >= 0)
            .collect();
        let (w, h) = (self.width as i32, self.height as i32);
        let mut out = Mask::new(self.width, self.height);
        for y in 0..h {
            for x in 0..w {
                let mut best = 0u8;
                'rows: for &(dy, half) in &spans {
                    let yy = y + dy;
                    if yy < 0 || yy >= h {
                        continue;
                    }
                    let row = &self.data[yy as usize * w as usize..(yy as usize + 1) * w as usize];
                    let (x0, x1) = ((x - half).max(0), (x + half).min(w - 1));
                    for &c in &row[x0 as usize..=x1 as usize] {
                        if c > best {
                            best = c;
                            if best == 255 {
                                break 'rows;
                            }
                        }
                    }
                }
                out.data[y as usize * w as usize + x as usize] = best;
            }
        }
        out
    }
}

/// `round(a * b / 255)`.
#[inline]
fn mul255(a: u8, b: u8) -> u8 {
    let t = u16::from(a) * u16::from(b) + 128;
    ((t + (t >> 8)) >> 8) as u8
}

/// Composite a straight-alpha color, scaled by `coverage`, over one
/// premultiplied RGBA8 pixel (`dst` is 4 bytes).
#[inline]
pub fn blend_over(dst: &mut [u8], color: [u8; 4], coverage: u8) {
    let a = mul255(color[3], coverage);
    if a == 0 {
        return;
    }
    let inv = 255 - a;
    for i in 0..3 {
        dst[i] = mul255(color[i], a).saturating_add(mul255(dst[i], inv));
    }
    dst[3] = a.saturating_add(mul255(dst[3], inv));
}

/// A premultiplied RGBA8 render target borrowed from the caller.
pub struct Canvas<'a> {
    data: &'a mut [u8],
    width: u32,
    height: u32,
}

impl<'a> Canvas<'a> {
    /// Wrap `data`, which must hold exactly `width * height` RGBA pixels.
    pub fn new(data: &'a mut [u8], width: u32, height: u32) -> Option<Self> {
        (data.len() == width as usize * height as usize * 4).then_some(Self {
            data,
            width,
            height,
        })
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// The raw premultiplied bytes.
    pub fn data(&self) -> &[u8] {
        self.data
    }

    /// Composite `mask`, tinted with `color`, with its top-left corner at
    /// `(x, y)`. Any part outside the canvas — including negative origins —
    /// is clipped.
    pub fn composite_mask(&mut self, mask: &Mask, x: i32, y: i32, color: Color) {
        let rgba = color.to_rgba8();
        if rgba[3] == 0 {
            return;
        }
        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = (x + mask.width as i32).min(self.width as i32);
        let y1 = (y + mask.height as i32).min(self.height as i32);
        for py in y0..y1 {
            let src_row = (py - y) as usize * mask.width as usize;
            let dst_row = py as usize * self.width as usize;
            for px in x0..x1 {
                let cov = mask.data[src_row + (px - x) as usize];
                if cov != 0 {
                    let i = (dst_row + px as usize) * 4;
                    blend_over(&mut self.data[i..i + 4], rgba, cov);
                }
            }
        }
    }

    /// Fill a rectangle (clipped) with `color`.
    pub fn fill_rect(&mut self, x: i32, y: i32, w: u32, h: u32, color: Color) {
        let rgba = color.to_rgba8();
        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = (x.saturating_add(w as i32)).min(self.width as i32);
        let y1 = (y.saturating_add(h as i32)).min(self.height as i32);
        for py in y0..y1 {
            for px in x0..x1 {
                let i = (py as usize * self.width as usize + px as usize) * 4;
                blend_over(&mut self.data[i..i + 4], rgba, 255);
            }
        }
    }
}

/// Convert premultiplied RGBA8 to straight (non-premultiplied) RGBA8.
pub fn unpremultiply(premultiplied: &[u8]) -> Vec<u8> {
    let mut out = premultiplied.to_vec();
    for px in out.as_chunks_mut::<4>().0.iter_mut() {
        let a = u32::from(px[3]);
        if a != 0 && a != 255 {
            for c in &mut px[..3] {
                *c = ((u32::from(*c) * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blend_over_transparent_is_premultiplied() {
        let mut px = [0u8; 4];
        blend_over(&mut px, [255, 255, 255, 255], 128);
        assert_eq!(px, [128, 128, 128, 128]);
    }

    #[test]
    fn blend_over_opaque_background() {
        let mut px = [0, 0, 255, 255];
        blend_over(&mut px, [255, 0, 0, 255], 255);
        assert_eq!(px, [255, 0, 0, 255]);
        let mut px = [0, 0, 255, 255];
        blend_over(&mut px, [255, 0, 0, 255], 128);
        assert_eq!(px[3], 255);
        assert!(
            (i32::from(px[0]) - 128).abs() <= 1 && (i32::from(px[2]) - 127).abs() <= 1,
            "{px:?}"
        );
    }

    #[test]
    fn zero_coverage_or_alpha_is_a_no_op() {
        let mut px = [10, 20, 30, 40];
        blend_over(&mut px, [255, 255, 255, 255], 0);
        blend_over(&mut px, [255, 255, 255, 0], 255);
        assert_eq!(px, [10, 20, 30, 40]);
    }

    #[test]
    fn unpremultiply_round_trips_blend() {
        let mut px = [0u8; 4];
        blend_over(&mut px, [200, 100, 50, 255], 128);
        let straight = unpremultiply(&px);
        assert_eq!(straight[3], 128);
        for (got, want) in straight[..3].iter().zip([200u8, 100, 50]) {
            assert!(
                (i32::from(*got) - i32::from(want)).abs() <= 2,
                "{straight:?}"
            );
        }
    }

    #[test]
    fn composite_clips_negative_origin() {
        let mut buf = vec![0u8; 4 * 4 * 4];
        let mut mask = Mask::new(4, 4);
        mask.data.fill(255);
        let mut canvas = Canvas::new(&mut buf, 4, 4).unwrap();
        canvas.composite_mask(&mask, -2, -3, Color::WHITE);
        // Covered: x in 0..2, y in 0..1.
        let covered = |x: usize, y: usize| buf[(y * 4 + x) * 4 + 3] == 255;
        assert!(covered(0, 0) && covered(1, 0));
        assert!(!covered(2, 0) && !covered(0, 1));
    }

    #[test]
    fn dilate_grows_by_radius() {
        let mut m = Mask::new(9, 9);
        m.data[4 * 9 + 4] = 255;
        let d = m.dilate(2.0);
        assert_eq!(d.get(4, 4), 255);
        assert_eq!(d.get(6, 4), 255);
        assert_eq!(d.get(4, 2), 255);
        assert_eq!(d.get(7, 4), 0);
        assert_eq!(d.get(6, 6), 0, "corner outside the disc");
        assert_eq!(m.dilate(0.0), m);
    }

    #[test]
    fn mask_add_is_source_over() {
        let mut m = Mask::new(1, 1);
        m.add(0, 0, 128);
        m.add(0, 0, 128);
        assert_eq!(m.get(0, 0), 192);
        m.add(5, 5, 255); // out of range: ignored
    }
}
