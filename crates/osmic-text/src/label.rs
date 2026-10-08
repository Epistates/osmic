//! Label candidates and priority-ordered, collision-free placement.
//!
//! Placement is deterministic: candidates are visited in
//! `(layer_rank, sort_key, input order)` order and each is accepted only if
//! its rectangles — derived from the real shaped text extents — do not
//! overlap anything already accepted. The same input always produces the
//! same output.
//!
//! Two placement modes exist:
//!
//! * [`LabelAnchor::Point`]: the label sits at (or, with `anchor`, next to)
//!   a point; one rectangle is reserved.
//! * [`LabelAnchor::Line`]: the label **follows the line**. Glyphs are laid
//!   out along the polyline, each rotated to the local tangent; the label
//!   is rejected where the line bends more than `max_angle_degrees`
//!   between neighbouring glyphs, is shorter than the text, or collides.
//!   One rectangle per glyph is reserved. Text is flipped so it always
//!   reads left to right.

use std::sync::Arc;

use osmic_core::Color;

use crate::collision::{CollisionIndex, Rect};
use crate::engine::{ShapedText, TextEngine};

/// Where a label is attached.
#[derive(Debug, Clone, PartialEq)]
pub enum LabelAnchor {
    /// A point, in canvas pixels.
    Point([f32; 2]),
    /// A polyline, in canvas pixels; the label follows it.
    Line(Vec<[f32; 2]>),
}

/// How a label looks and behaves.
#[derive(Debug, Clone, PartialEq)]
pub struct LabelStyle {
    /// Font size in pixels.
    pub font_size: f32,
    pub color: Color,
    pub halo_color: Color,
    /// Halo radius in pixels; `0` disables the halo. Drawn at most
    /// [`crate::max_halo_width`] of `font_size` wide, like MapLibre.
    pub halo_width: f32,
    /// The point of the text box (`[0,0]` top-left … `[1,1]` bottom-right)
    /// that sits on the anchor. Point labels only.
    pub anchor: [f32; 2],
    /// Offset from the anchor in pixels. Point labels only.
    pub offset: [f32; 2],
    /// Clearance kept around the label, in pixels.
    pub padding: f32,
    /// Skip the collision test (the label still blocks later ones).
    pub allow_overlap: bool,
    /// Largest angle between neighbouring glyphs of a line label.
    pub max_angle_degrees: f32,
}

impl Default for LabelStyle {
    fn default() -> Self {
        Self {
            font_size: 12.0,
            color: Color::BLACK,
            halo_color: Color::TRANSPARENT,
            halo_width: 0.0,
            anchor: [0.5, 0.5],
            offset: [0.0, 0.0],
            padding: 2.0,
            allow_overlap: false,
            max_angle_degrees: 45.0,
        }
    }
}

impl LabelAnchor {
    /// The anchor with every point passed through `f` (for example a
    /// tile-to-screen transform).
    pub fn map(&self, f: impl Fn([f32; 2]) -> [f32; 2]) -> Self {
        match self {
            Self::Point(p) => Self::Point(f(*p)),
            Self::Line(line) => Self::Line(line.iter().map(|p| f(*p)).collect()),
        }
    }
}

impl LabelStyle {
    /// The style with every length (font size, halo, offset, padding)
    /// multiplied by `factor`, for example a device pixel ratio.
    pub fn scaled(&self, factor: f32) -> Self {
        Self {
            font_size: self.font_size * factor,
            halo_width: self.halo_width * factor,
            offset: [self.offset[0] * factor, self.offset[1] * factor],
            padding: self.padding * factor,
            ..self.clone()
        }
    }
}

/// A label that may or may not fit.
#[derive(Debug, Clone, PartialEq)]
pub struct LabelCandidate {
    pub text: String,
    pub anchor: LabelAnchor,
    pub style: LabelStyle,
    /// Primary priority: lower ranks are placed first and win collisions.
    pub layer_rank: u32,
    /// Secondary priority within a rank: lower sort keys are placed first.
    pub sort_key: f32,
}

/// One glyph of a placed label.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlacedGlyph {
    /// Index into the label's [`ShapedText::glyphs`].
    pub index: usize,
    /// Where the centre of the glyph's box lands on the canvas.
    pub center: [f32; 2],
    /// Clockwise rotation in radians (canvas y points down).
    pub angle: f32,
}

/// A label that was accepted.
#[derive(Debug, Clone, PartialEq)]
pub struct PlacedLabel {
    /// Index of the originating [`LabelCandidate`].
    pub candidate: usize,
    pub glyphs: Vec<PlacedGlyph>,
    /// Bounding box of the label on the canvas (without padding).
    pub bounds: Rect,
}

/// Places labels into a viewport without overlaps.
pub struct LabelPlacer {
    viewport: Rect,
    index: CollisionIndex,
}

impl LabelPlacer {
    /// A placer for a canvas covering `viewport`. Labels entirely outside
    /// it are dropped.
    pub fn new(viewport: Rect) -> Self {
        Self {
            viewport,
            index: CollisionIndex::new(),
        }
    }

    /// Keep labels away from `rect` (for example a UI panel).
    pub fn reserve(&mut self, rect: Rect) {
        self.index.insert(rect);
    }

    /// Place as many `candidates` as fit, highest priority first. The
    /// result is in placement (priority) order.
    pub fn place(
        &mut self,
        engine: &mut TextEngine,
        candidates: &[LabelCandidate],
    ) -> Vec<PlacedLabel> {
        let mut order: Vec<usize> = (0..candidates.len()).collect();
        order.sort_by(|&a, &b| {
            let (ca, cb) = (&candidates[a], &candidates[b]);
            ca.layer_rank
                .cmp(&cb.layer_rank)
                .then(ca.sort_key.total_cmp(&cb.sort_key))
                .then(a.cmp(&b))
        });
        let mut placed = Vec::new();
        for i in order {
            let cand = &candidates[i];
            let shaped = engine.shape(&cand.text, cand.style.font_size);
            if shaped.is_empty() {
                continue;
            }
            let result = match &cand.anchor {
                LabelAnchor::Point(p) => self.place_point(cand, i, &shaped, *p),
                LabelAnchor::Line(line) => self.place_line(cand, i, &shaped, line),
            };
            placed.extend(result);
        }
        placed
    }

    fn place_point(
        &mut self,
        cand: &LabelCandidate,
        candidate: usize,
        shaped: &ShapedText,
        at: [f32; 2],
    ) -> Option<PlacedLabel> {
        let s = &cand.style;
        if !(at[0].is_finite() && at[1].is_finite()) {
            return None;
        }
        // Whole-pixel origin keeps text crisp.
        let ox = (at[0] + s.offset[0] - shaped.width * s.anchor[0]).round();
        let oy = (at[1] + s.offset[1] - shaped.height * s.anchor[1]).round();
        let bounds = Rect::new(ox, oy, ox + shaped.width, oy + shaped.height);
        if !bounds.overlaps(&self.viewport) {
            return None;
        }
        let padded = bounds.inflate(s.padding);
        if !s.allow_overlap && self.index.collides(&padded) {
            return None;
        }
        self.index.insert(padded);
        let glyphs = (0..shaped.glyphs.len())
            .map(|index| {
                let c = shaped.glyph_center(index);
                PlacedGlyph {
                    index,
                    center: [ox + c[0], oy + c[1]],
                    angle: 0.0,
                }
            })
            .collect();
        Some(PlacedLabel {
            candidate,
            glyphs,
            bounds,
        })
    }

    fn place_line(
        &mut self,
        cand: &LabelCandidate,
        candidate: usize,
        shaped: &Arc<ShapedText>,
        line: &[[f32; 2]],
    ) -> Option<PlacedLabel> {
        let line: Vec<[f32; 2]> = line
            .iter()
            .copied()
            .filter(|p| p[0].is_finite() && p[1].is_finite())
            .collect();
        let mut pieces = clip_polyline(&line, &self.viewport);
        // Prefer the longest visible stretch; the sort is stable so equal
        // lengths keep input order.
        let mut lengths: Vec<(f32, Vec<[f32; 2]>)> =
            pieces.drain(..).map(|p| (polyline_length(&p), p)).collect();
        lengths.sort_by(|a, b| b.0.total_cmp(&a.0));

        let s = &cand.style;
        for (length, piece) in &lengths {
            if *length < shaped.width {
                continue;
            }
            for fraction in [0.5, 0.3, 0.7, 0.15, 0.85] {
                let center =
                    (length * fraction).clamp(shaped.width / 2.0, length - shaped.width / 2.0);
                let Some(glyphs) = layout_on_line(piece, center, shaped, s.max_angle_degrees)
                else {
                    continue;
                };
                let rects: Vec<Rect> = glyphs
                    .iter()
                    .map(|g| glyph_rect(shaped, g).inflate(s.padding))
                    .collect();
                if !s.allow_overlap && self.index.collides_any(&rects) {
                    continue;
                }
                let bounds = rects
                    .iter()
                    .skip(1)
                    .fold(rects[0], |acc, r| acc.union(r))
                    .inflate(-s.padding);
                for r in rects {
                    self.index.insert(r);
                }
                return Some(PlacedLabel {
                    candidate,
                    glyphs,
                    bounds,
                });
            }
        }
        None
    }
}

/// Axis-aligned bounds of a rotated glyph box.
fn glyph_rect(shaped: &ShapedText, g: &PlacedGlyph) -> Rect {
    let adv = shaped.glyphs[g.index].advance;
    let (hw, hh) = (adv / 2.0, shaped.height / 2.0);
    let (s, c) = g.angle.sin_cos();
    let ex = hw * c.abs() + hh * s.abs();
    let ey = hw * s.abs() + hh * c.abs();
    Rect::new(
        g.center[0] - ex,
        g.center[1] - ey,
        g.center[0] + ex,
        g.center[1] + ey,
    )
}

/// Lay `shaped` along `line`, centred `center` pixels from its start.
fn layout_on_line(
    line: &[[f32; 2]],
    center: f32,
    shaped: &ShapedText,
    max_angle_degrees: f32,
) -> Option<Vec<PlacedGlyph>> {
    let cum = cumulative_lengths(line);
    let total = *cum.last()?;
    let start = center - shaped.width / 2.0;
    let a = point_at(line, &cum, start);
    let b = point_at(line, &cum, start + shaped.width);
    // Always read left to right.
    let flip = b[0] < a[0];
    let (poly, cum, start) = if flip {
        let rev: Vec<[f32; 2]> = line.iter().rev().copied().collect();
        let rev_cum = cumulative_lengths(&rev);
        (rev, rev_cum, total - (start + shaped.width))
    } else {
        (line.to_vec(), cum, start)
    };
    let total = *cum.last()?;
    let max_angle = max_angle_degrees.to_radians();

    let mut glyphs = Vec::with_capacity(shaped.glyphs.len());
    let mut prev_angle: Option<f32> = None;
    for (index, g) in shaped.glyphs.iter().enumerate() {
        let d = (start + g.x + g.advance / 2.0).clamp(0.0, total);
        let half = (g.advance / 2.0).max(0.5);
        let p0 = point_at(&poly, &cum, (d - half).max(0.0));
        let p1 = point_at(&poly, &cum, (d + half).min(total));
        let angle = (p1[1] - p0[1]).atan2(p1[0] - p0[0]);
        if let Some(prev) = prev_angle
            && angle_difference(angle, prev) > max_angle
        {
            return None;
        }
        prev_angle = Some(angle);
        glyphs.push(PlacedGlyph {
            index,
            center: point_at(&poly, &cum, d),
            angle,
        });
    }
    Some(glyphs)
}

fn angle_difference(a: f32, b: f32) -> f32 {
    let mut d = (a - b).abs() % std::f32::consts::TAU;
    if d > std::f32::consts::PI {
        d = std::f32::consts::TAU - d;
    }
    d
}

fn cumulative_lengths(line: &[[f32; 2]]) -> Vec<f32> {
    let mut cum = Vec::with_capacity(line.len());
    let mut acc = 0.0;
    for (i, p) in line.iter().enumerate() {
        if i > 0 {
            acc += (p[0] - line[i - 1][0]).hypot(p[1] - line[i - 1][1]);
        }
        cum.push(acc);
    }
    cum
}

fn polyline_length(line: &[[f32; 2]]) -> f32 {
    cumulative_lengths(line).last().copied().unwrap_or(0.0)
}

/// The point `d` pixels along the polyline (clamped to its ends).
fn point_at(line: &[[f32; 2]], cum: &[f32], d: f32) -> [f32; 2] {
    let last = line.len() - 1;
    if d <= 0.0 || last == 0 {
        return line[0];
    }
    if d >= cum[last] {
        return line[last];
    }
    let i = cum.partition_point(|&c| c <= d).clamp(1, last);
    let seg = cum[i] - cum[i - 1];
    let t = if seg > 0.0 {
        (d - cum[i - 1]) / seg
    } else {
        0.0
    };
    [
        line[i - 1][0] + (line[i][0] - line[i - 1][0]) * t,
        line[i - 1][1] + (line[i][1] - line[i - 1][1]) * t,
    ]
}

/// Liang–Barsky clip of one segment to `rect`.
fn clip_segment(a: [f32; 2], b: [f32; 2], rect: &Rect) -> Option<([f32; 2], [f32; 2])> {
    let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
    let (mut t0, mut t1) = (0.0f32, 1.0f32);
    for (p, q) in [
        (-dx, a[0] - rect.min[0]),
        (dx, rect.max[0] - a[0]),
        (-dy, a[1] - rect.min[1]),
        (dy, rect.max[1] - a[1]),
    ] {
        if p == 0.0 {
            if q < 0.0 {
                return None;
            }
        } else {
            let r = q / p;
            if p < 0.0 {
                if r > t1 {
                    return None;
                }
                t0 = t0.max(r);
            } else {
                if r < t0 {
                    return None;
                }
                t1 = t1.min(r);
            }
        }
    }
    Some((
        [a[0] + dx * t0, a[1] + dy * t0],
        [a[0] + dx * t1, a[1] + dy * t1],
    ))
}

/// The parts of `line` inside `rect`, as separate polylines.
pub fn clip_polyline(line: &[[f32; 2]], rect: &Rect) -> Vec<Vec<[f32; 2]>> {
    let mut pieces: Vec<Vec<[f32; 2]>> = Vec::new();
    for w in line.windows(2) {
        if let Some((a, b)) = clip_segment(w[0], w[1], rect) {
            match pieces.last_mut() {
                Some(piece) if piece.last() == Some(&a) => piece.push(b),
                _ => pieces.push(vec![a, b]),
            }
        }
    }
    pieces
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::test_engine;

    fn viewport() -> Rect {
        Rect::new(0.0, 0.0, 400.0, 300.0)
    }

    fn cand(text: &str, at: [f32; 2], rank: u32) -> LabelCandidate {
        LabelCandidate {
            text: text.into(),
            anchor: LabelAnchor::Point(at),
            style: LabelStyle {
                padding: 0.0,
                ..LabelStyle::default()
            },
            layer_rank: rank,
            sort_key: 0.0,
        }
    }

    #[test]
    fn rect_comes_from_real_text_extents() {
        let mut engine = test_engine();
        let mut placer = LabelPlacer::new(viewport());
        let cands = [
            cand("Hi", [100.0, 100.0], 0),
            cand("A considerably longer label", [100.0, 200.0], 0),
        ];
        let placed = placer.place(&mut engine, &cands);
        assert_eq!(placed.len(), 2);
        let short = engine.shape("Hi", 12.0);
        let long = engine.shape("A considerably longer label", 12.0);
        assert!(long.width > short.width * 4.0);
        for (p, shaped) in placed.iter().zip([&short, &long]) {
            assert!(
                (p.bounds.width() - shaped.width).abs() < 0.01,
                "{:?}",
                p.bounds
            );
            assert!((p.bounds.height() - shaped.height).abs() < 0.01);
            // Centered on the anchor.
            let cx = (p.bounds.min[0] + p.bounds.max[0]) / 2.0;
            assert!((cx - 100.0).abs() <= 0.5 + shaped.width % 1.0);
        }
    }

    #[test]
    fn overlapping_labels_lose_to_higher_priority() {
        let mut engine = test_engine();
        let mut placer = LabelPlacer::new(viewport());
        // Same spot; the low-priority one is listed first.
        let cands = [
            cand("Low priority", [100.0, 100.0], 5),
            cand("High priority", [105.0, 102.0], 1),
        ];
        let placed = placer.place(&mut engine, &cands);
        assert_eq!(placed.len(), 1);
        assert_eq!(placed[0].candidate, 1);
    }

    #[test]
    fn sort_key_breaks_ties_within_a_rank_then_input_order() {
        let mut engine = test_engine();
        let mut a = cand("Alpha", [100.0, 100.0], 0);
        let mut b = cand("Beta", [100.0, 100.0], 0);
        a.sort_key = 2.0;
        b.sort_key = 1.0;
        let placed = LabelPlacer::new(viewport()).place(&mut engine, &[a.clone(), b.clone()]);
        assert_eq!(
            placed.iter().map(|p| p.candidate).collect::<Vec<_>>(),
            vec![1]
        );
        // Equal keys: first in input order wins.
        a.sort_key = 1.0;
        let placed = LabelPlacer::new(viewport()).place(&mut engine, &[a, b]);
        assert_eq!(placed[0].candidate, 0);
    }

    #[test]
    fn placement_is_deterministic() {
        let cands: Vec<LabelCandidate> = (0..60)
            .map(|i| {
                cand(
                    &format!("Place {i}"),
                    [(i * 37 % 380) as f32, (i * 53 % 290) as f32],
                    (i % 3) as u32,
                )
            })
            .collect();
        let run = || {
            let mut engine = test_engine();
            LabelPlacer::new(viewport()).place(&mut engine, &cands)
        };
        let first = run();
        assert!(
            !first.is_empty() && first.len() < cands.len(),
            "some, but not all, fit"
        );
        assert_eq!(first, run());
    }

    #[test]
    fn no_two_placed_labels_overlap() {
        let mut engine = test_engine();
        let cands: Vec<LabelCandidate> = (0..80)
            .map(|i| {
                cand(
                    "Crowded",
                    [(i * 29 % 380) as f32 + 10.0, (i * 41 % 280) as f32 + 10.0],
                    0,
                )
            })
            .collect();
        let placed = LabelPlacer::new(viewport()).place(&mut engine, &cands);
        for (i, a) in placed.iter().enumerate() {
            for b in &placed[i + 1..] {
                assert!(!a.bounds.overlaps(&b.bounds), "{a:?} overlaps {b:?}");
            }
        }
    }

    #[test]
    fn allow_overlap_skips_the_test_but_still_blocks_others() {
        let mut engine = test_engine();
        let mut first = cand("Anchor", [100.0, 100.0], 0);
        first.style.allow_overlap = true;
        let mut second = cand("Anchor", [100.0, 100.0], 1);
        second.style.allow_overlap = true;
        let third = cand("Anchor", [100.0, 100.0], 2);
        let placed = LabelPlacer::new(viewport()).place(&mut engine, &[first, second, third]);
        assert_eq!(placed.len(), 2);
    }

    #[test]
    fn padding_keeps_a_gap() {
        let mut engine = test_engine();
        let w = engine.shape("Gap", 12.0).width;
        let mut a = cand("Gap", [100.0, 100.0], 0);
        let mut b = cand("Gap", [100.0 + w + 1.0, 100.0], 0);
        assert_eq!(
            LabelPlacer::new(viewport())
                .place(&mut engine, &[a.clone(), b.clone()])
                .len(),
            2
        );
        a.style.padding = 4.0;
        b.style.padding = 4.0;
        assert_eq!(
            LabelPlacer::new(viewport())
                .place(&mut engine, &[a, b])
                .len(),
            1
        );
    }

    #[test]
    fn labels_outside_the_viewport_are_dropped() {
        let mut engine = test_engine();
        let cands = [
            cand("Off", [-500.0, 100.0], 0),
            cand("Edge", [0.0, 100.0], 0),
            cand("In", [200.0, 100.0], 0),
            cand("NaN", [f32::NAN, 1.0], 0),
        ];
        let placed = LabelPlacer::new(viewport()).place(&mut engine, &cands);
        assert_eq!(
            placed.iter().map(|p| p.candidate).collect::<Vec<_>>(),
            vec![1, 2]
        );
    }

    #[test]
    fn reserved_regions_block_labels() {
        let mut engine = test_engine();
        let mut placer = LabelPlacer::new(viewport());
        placer.reserve(Rect::new(80.0, 80.0, 120.0, 120.0));
        assert!(
            placer
                .place(&mut engine, &[cand("Blocked", [100.0, 100.0], 0)])
                .is_empty()
        );
    }

    #[test]
    fn anchor_and_offset_position_the_box() {
        let mut engine = test_engine();
        let mut c = cand("Anchored", [100.0, 100.0], 0);
        c.style.anchor = [0.0, 1.0]; // bottom-left corner on the point
        let p = &LabelPlacer::new(viewport()).place(&mut engine, &[c.clone()])[0];
        assert_eq!(p.bounds.min[0], 100.0);
        assert!((p.bounds.max[1] - 100.0).abs() <= 0.5, "{:?}", p.bounds);
        c.style.offset = [10.0, -5.0];
        let p = &LabelPlacer::new(viewport()).place(&mut engine, &[c])[0];
        assert_eq!(p.bounds.min[0], 110.0);
        assert!((p.bounds.max[1] - 95.0).abs() <= 0.5, "{:?}", p.bounds);
    }

    fn line_cand(text: &str, line: Vec<[f32; 2]>) -> LabelCandidate {
        LabelCandidate {
            text: text.into(),
            anchor: LabelAnchor::Line(line),
            style: LabelStyle {
                padding: 0.0,
                max_angle_degrees: 30.0,
                ..LabelStyle::default()
            },
            layer_rank: 0,
            sort_key: 0.0,
        }
    }

    #[test]
    fn line_label_follows_a_diagonal() {
        let mut engine = test_engine();
        let c = line_cand("Diagonal road", vec![[20.0, 20.0], [320.0, 220.0]]);
        let placed = LabelPlacer::new(viewport()).place(&mut engine, &[c]);
        assert_eq!(placed.len(), 1);
        let expected = (200.0f32).atan2(300.0);
        for g in &placed[0].glyphs {
            assert!((g.angle - expected).abs() < 1e-3, "{}", g.angle);
        }
        // Glyph centres lie on the line y = 20 + (x - 20) * 2/3.
        for g in &placed[0].glyphs {
            let y = 20.0 + (g.center[0] - 20.0) * 200.0 / 300.0;
            assert!((g.center[1] - y).abs() < 0.01);
        }
        // Centres advance left to right.
        assert!(
            placed[0]
                .glyphs
                .windows(2)
                .all(|w| w[0].center[0] < w[1].center[0])
        );
    }

    #[test]
    fn line_labels_are_flipped_to_read_left_to_right() {
        let mut engine = test_engine();
        let forward = line_cand("Street", vec![[20.0, 200.0], [300.0, 100.0]]);
        let backward = line_cand("Street", vec![[300.0, 100.0], [20.0, 200.0]]);
        let a = LabelPlacer::new(viewport()).place(&mut engine, &[forward]);
        let b = LabelPlacer::new(viewport()).place(&mut engine, &[backward]);
        for g in a[0].glyphs.iter().chain(&b[0].glyphs) {
            assert!(
                g.angle.abs() < std::f32::consts::FRAC_PI_2,
                "upside down: {}",
                g.angle
            );
        }
        for (ga, gb) in a[0].glyphs.iter().zip(&b[0].glyphs) {
            assert!((ga.center[0] - gb.center[0]).abs() < 0.01);
            assert!((ga.center[1] - gb.center[1]).abs() < 0.01);
        }
    }

    #[test]
    fn line_label_follows_a_gentle_curve() {
        let mut engine = test_engine();
        // An arc of radius 400: gentle enough for the 30 degree limit.
        let line: Vec<[f32; 2]> = (0..=40)
            .map(|i| {
                let t = -0.5 + i as f32 / 40.0;
                [200.0 + 400.0 * t.sin(), 400.0 - 400.0 * t.cos()]
            })
            .collect();
        let placed =
            LabelPlacer::new(viewport()).place(&mut engine, &[line_cand("Curved avenue", line)]);
        assert_eq!(placed.len(), 1);
        let angles: Vec<f32> = placed[0].glyphs.iter().map(|g| g.angle).collect();
        assert!(
            angles.windows(2).any(|w| (w[0] - w[1]).abs() > 1e-4),
            "glyphs must rotate along the curve"
        );
    }

    #[test]
    fn sharp_bends_and_short_lines_are_rejected() {
        let mut engine = test_engine();
        // A hairpin right in the middle of the label.
        let hairpin = line_cand(
            "Switchback road",
            vec![
                [100.0, 100.0],
                [160.0, 100.0],
                [160.0, 106.0],
                [100.0, 112.0],
            ],
        );
        assert!(
            LabelPlacer::new(viewport())
                .place(&mut engine, &[hairpin])
                .is_empty()
        );
        let short = line_cand(
            "A very long street name",
            vec![[100.0, 100.0], [120.0, 100.0]],
        );
        assert!(
            LabelPlacer::new(viewport())
                .place(&mut engine, &[short])
                .is_empty()
        );
    }

    #[test]
    fn line_labels_participate_in_collisions() {
        let mut engine = test_engine();
        let mut first = line_cand("Main Street", vec![[20.0, 100.0], [300.0, 100.0]]);
        first.layer_rank = 0;
        let mut second = line_cand("Other Street", vec![[40.0, 100.0], [320.0, 100.0]]);
        second.layer_rank = 1;
        let placed =
            LabelPlacer::new(viewport()).place(&mut engine, &[second.clone(), first.clone()]);
        // Both are tried at several positions; the second may find a free
        // stretch but must never overlap the first.
        let first_bounds = placed.iter().find(|p| p.candidate == 1).unwrap().bounds;
        if let Some(other) = placed.iter().find(|p| p.candidate == 0) {
            assert!(!other.bounds.overlaps(&first_bounds));
        }
    }

    #[test]
    fn anchors_and_styles_transform() {
        let anchor = LabelAnchor::Line(vec![[1.0, 2.0], [3.0, 4.0]]);
        assert_eq!(
            anchor.map(|p| [p[0] * 2.0 + 1.0, p[1] * 2.0]),
            LabelAnchor::Line(vec![[3.0, 4.0], [7.0, 8.0]])
        );
        let style = LabelStyle {
            font_size: 10.0,
            halo_width: 1.0,
            padding: 2.0,
            offset: [1.0, 2.0],
            ..LabelStyle::default()
        };
        let s2 = style.scaled(2.0);
        assert_eq!(
            (s2.font_size, s2.halo_width, s2.padding, s2.offset),
            (20.0, 2.0, 4.0, [2.0, 4.0])
        );
        assert_eq!(s2.max_angle_degrees, style.max_angle_degrees);
    }

    #[test]
    fn clipping_keeps_only_the_visible_stretch() {
        let pieces = clip_polyline(
            &[[-100.0, 50.0], [500.0, 50.0]],
            &Rect::new(0.0, 0.0, 400.0, 300.0),
        );
        assert_eq!(pieces, vec![vec![[0.0, 50.0], [400.0, 50.0]]]);
        let pieces = clip_polyline(
            &[[-100.0, -100.0], [-50.0, -50.0]],
            &Rect::new(0.0, 0.0, 10.0, 10.0),
        );
        assert!(pieces.is_empty());
    }
}
