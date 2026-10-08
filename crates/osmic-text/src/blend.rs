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
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Coverage, row-major, `width * height` bytes.
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

    /// The shape grown by a disc of `radius` pixels: the halo around text,
    /// produced in one pass instead of drawing offset copies.
    ///
    /// Each output pixel is the larger of its own coverage and the halo's,
    /// which is 1 within `radius` of the glyph edge and falls off linearly
    /// over one pixel (anti-aliasing). Distances come from an exact
    /// Euclidean distance transform, so the cost is linear in the number
    /// of pixels whatever the radius.
    ///
    /// The mask keeps its size, so callers should allocate at least
    /// `radius.ceil()` pixels of padding around the glyphs.
    pub fn dilate(&self, radius: f32) -> Mask {
        if radius.is_nan() || radius <= 0.0 || self.width == 0 || self.height == 0 {
            return self.clone();
        }
        let dist2 = self.squared_distances();
        let radius = f64::from(radius);
        let mut out = self.clone();
        for (o, d2) in out.data.iter_mut().zip(&dist2) {
            // The distance between pixel centres, minus the half pixel to
            // the source pixel's edge, plus half a pixel of coverage ramp.
            let halo = (radius + 1.0 - d2.sqrt()).clamp(0.0, 1.0);
            *o = (*o).max((halo * 255.0).round() as u8);
        }
        out
    }

    /// Squared distance from every pixel centre to the glyph shape.
    ///
    /// Pixels at least half covered are inside (distance 0); fainter
    /// anti-aliased pixels are a fraction of a pixel away, so thin strokes
    /// still get a halo; empty pixels are outside. Computed with the
    /// separable Felzenszwalb–Huttenlocher transform: columns, then rows.
    fn squared_distances(&self) -> Vec<f64> {
        let (w, h) = (self.width as usize, self.height as usize);
        let mut grid: Vec<f64> = self
            .data
            .iter()
            .map(|&c| match c {
                0 => f64::INFINITY,
                128.. => 0.0,
                c => (f64::from(128 - c) / 255.0).powi(2),
            })
            .collect();
        let mut scratch = Edt1d::default();
        let (mut line, mut out) = (vec![0.0; h], vec![0.0; h]);
        for x in 0..w {
            for (y, v) in line.iter_mut().enumerate() {
                *v = grid[y * w + x];
            }
            scratch.run(&line, &mut out);
            for (y, v) in out.iter().enumerate() {
                grid[y * w + x] = *v;
            }
        }
        let mut row_out = vec![0.0; w];
        for row in grid.chunks_exact_mut(w) {
            scratch.run(row, &mut row_out);
            row.copy_from_slice(&row_out);
        }
        grid
    }
}

/// Scratch space for the one-dimensional squared distance transform.
#[derive(Default)]
struct Edt1d {
    /// Sample positions of the parabolas in the lower envelope.
    sites: Vec<usize>,
    /// Where each envelope parabola starts to be the lowest.
    starts: Vec<f64>,
}

impl Edt1d {
    /// `out[x] = min over q of (x - q)^2 + f[q]`, skipping infinite `f[q]`
    /// (everything is infinite if all of `f` is).
    fn run(&mut self, f: &[f64], out: &mut [f64]) {
        self.sites.clear();
        self.starts.clear();
        for (q, &fq) in f.iter().enumerate() {
            if !fq.is_finite() {
                continue;
            }
            let qf = q as f64;
            let mut start = f64::NEG_INFINITY;
            while let Some(&p) = self.sites.last() {
                let pf = p as f64;
                let s = ((fq + qf * qf) - (f[p] + pf * pf)) / (2.0 * (qf - pf));
                if s <= *self.starts.last().expect("parallel to sites") {
                    self.sites.pop();
                    self.starts.pop();
                } else {
                    start = s;
                    break;
                }
            }
            self.sites.push(q);
            self.starts.push(start);
        }
        if self.sites.is_empty() {
            out.fill(f64::INFINITY);
            return;
        }
        let mut k = 0;
        for (x, o) in out.iter_mut().enumerate() {
            let xf = x as f64;
            while k + 1 < self.sites.len() && self.starts[k + 1] <= xf {
                k += 1;
            }
            let p = self.sites[k];
            *o = (xf - p as f64).powi(2) + f[p];
        }
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

    /// Width in pixels.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Height in pixels.
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
        // In `i64`: a mask far off-canvas must clip, not overflow.
        let (x, y) = (i64::from(x), i64::from(y));
        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = (x + i64::from(mask.width)).min(i64::from(self.width));
        let y1 = (y + i64::from(mask.height)).min(i64::from(self.height));
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
        let (x, y) = (i64::from(x), i64::from(y));
        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = (x + i64::from(w)).min(i64::from(self.width));
        let y1 = (y + i64::from(h)).min(i64::from(self.height));
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
    fn composite_far_off_canvas_is_a_no_op() {
        let mut buf = vec![0u8; 4 * 4 * 4];
        let mut mask = Mask::new(4, 4);
        mask.data.fill(255);
        let mut canvas = Canvas::new(&mut buf, 4, 4).unwrap();
        canvas.composite_mask(&mask, i32::MAX - 1, i32::MAX - 1, Color::WHITE);
        canvas.composite_mask(&mask, i32::MIN, i32::MIN, Color::WHITE);
        assert!(buf.iter().all(|&b| b == 0));
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
        let corner = d.get(6, 6);
        assert!(corner > 0 && corner < 128, "corner only clipped: {corner}");
        assert_eq!(d.get(5, 5), 255);
        assert_eq!(m.dilate(0.0), m);
        assert_eq!(m.dilate(f32::NAN), m);
    }

    #[test]
    fn distance_transform_matches_brute_force() {
        let (w, h) = (23u32, 17u32);
        let mut m = Mask::new(w, h);
        let mut s = 0x9E37_79B9u32;
        for c in &mut m.data {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            *c = if s.is_multiple_of(11) {
                (s >> 8) as u8
            } else {
                0
            };
        }
        let fast = m.squared_distances();
        for y in 0..h as usize {
            for x in 0..w as usize {
                let mut best = f64::INFINITY;
                for (i, &c) in m.data.iter().enumerate() {
                    let f = match c {
                        0 => continue,
                        128.. => 0.0,
                        c => (f64::from(128 - c) / 255.0).powi(2),
                    };
                    let (qx, qy) = ((i % w as usize) as f64, (i / w as usize) as f64);
                    best = best.min((x as f64 - qx).powi(2) + (y as f64 - qy).powi(2) + f);
                }
                let got = fast[y * w as usize + x];
                assert!((got - best).abs() < 1e-9, "({x},{y}): {got} != {best}");
            }
        }
    }

    #[test]
    fn huge_halo_radii_are_linear_time() {
        let mut m = Mask::new(1024, 1024);
        m.data[512 * 1024 + 512] = 255;
        let start = std::time::Instant::now();
        let d = m.dilate(1.0e6);
        assert!(d.data.iter().all(|&c| c == 255));
        // A disc dilation would be ~10^12 operations; this is 2 * 10^6.
        assert!(start.elapsed().as_secs() < 10);
        assert_eq!(
            Mask::new(8, 8).dilate(3.0),
            Mask::new(8, 8),
            "no shape, no halo"
        );
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
