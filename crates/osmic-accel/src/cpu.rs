//! CPU unit kernels. These mirror `src/kernels/geometry_ops.metal` operation
//! for operation (same evaluation order, same capacity checks) so the two
//! backends agree up to f32 rounding of the intersection points.
//!
//! - Rings: Sutherland-Hodgman against the four clip edges (left, right,
//!   bottom, top), skipping edges the ring's bounding box proves irrelevant.
//! - Lines: Liang-Barsky per segment, emitting one output part per contiguous
//!   run inside the clip box.

use crate::clip::{UnitResults, UnitView};
use crate::prepare::{Bounds, Prepared, UnitKind};

/// A Sutherland-Hodgman stage would have written more than the allowed
/// number of vertices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Overflow;

/// Clip edges in application order: (axis, sign, value selector).
/// A vertex `p` is inside an edge when `(p[axis] - value) * sign >= 0`.
fn edges(b: &Bounds) -> [(usize, f32, f32); 4] {
    [
        (0, 1.0, b.min_x),
        (0, -1.0, b.max_x),
        (1, 1.0, b.min_y),
        (1, -1.0, b.max_y),
    ]
}

/// One Sutherland-Hodgman stage. `dst` is cleared first; fails if more than
/// `cap` vertices would be produced.
fn clip_stage(
    src: &[[f32; 2]],
    dst: &mut Vec<[f32; 2]>,
    (axis, sign, value): (usize, f32, f32),
    cap: usize,
) -> Result<(), Overflow> {
    dst.clear();
    let Some(&last) = src.last() else {
        return Ok(());
    };
    let mut prev = last;
    let mut prev_d = (prev[axis] - value) * sign;
    for &curr in src {
        let curr_d = (curr[axis] - value) * sign;
        let curr_in = curr_d >= 0.0;
        let prev_in = prev_d >= 0.0;
        if curr_in != prev_in {
            if dst.len() >= cap {
                return Err(Overflow);
            }
            let t = prev_d / (prev_d - curr_d);
            let mut p = [
                prev[0] + t * (curr[0] - prev[0]),
                prev[1] + t * (curr[1] - prev[1]),
            ];
            p[axis] = value;
            dst.push(p);
        }
        if curr_in {
            if dst.len() >= cap {
                return Err(Overflow);
            }
            dst.push(curr);
        }
        prev = curr;
        prev_d = curr_d;
    }
    Ok(())
}

/// Where the ring currently being clipped lives.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Loc {
    Input,
    Scratch,
    Out,
}

/// Clip a closed ring (open representation) to `bounds`.
///
/// `out` receives the result (empty if fewer than 3 vertices survive).
/// `cap` bounds every intermediate stage; pass `usize::MAX` for no limit.
pub(crate) fn clip_ring(
    input: &[[f32; 2]],
    bounds: &Bounds,
    cap: usize,
    out: &mut Vec<[f32; 2]>,
    scratch: &mut Vec<[f32; 2]>,
) -> Result<(), Overflow> {
    out.clear();
    let Some(&first) = input.first() else {
        return Ok(());
    };
    let (mut lo, mut hi) = (first, first);
    for p in input {
        lo = [lo[0].min(p[0]), lo[1].min(p[1])];
        hi = [hi[0].max(p[0]), hi[1].max(p[1])];
    }
    // Entirely outside one edge: every stage would drop everything.
    if hi[0] < bounds.min_x || lo[0] > bounds.max_x || hi[1] < bounds.min_y || lo[1] > bounds.max_y
    {
        return Ok(());
    }
    let needed = [
        lo[0] < bounds.min_x,
        hi[0] > bounds.max_x,
        lo[1] < bounds.min_y,
        hi[1] > bounds.max_y,
    ];
    let stages = needed.iter().filter(|&&n| n).count();

    if stages == 0 {
        if input.len() > cap {
            return Err(Overflow);
        }
        out.extend_from_slice(input);
        return Ok(());
    }

    // Ping-pong so the last stage lands in `out`.
    let mut remaining = stages;
    let mut loc = Loc::Input;
    for (stage, edge) in edges(bounds).into_iter().enumerate() {
        if !needed[stage] {
            continue;
        }
        remaining -= 1;
        loc = match loc {
            Loc::Input if remaining % 2 == 0 => {
                clip_stage(input, out, edge, cap)?;
                Loc::Out
            }
            Loc::Input => {
                clip_stage(input, scratch, edge, cap)?;
                Loc::Scratch
            }
            Loc::Scratch => {
                clip_stage(scratch, out, edge, cap)?;
                Loc::Out
            }
            Loc::Out => {
                clip_stage(out, scratch, edge, cap)?;
                Loc::Scratch
            }
        };
        let produced = if loc == Loc::Out {
            out.len()
        } else {
            scratch.len()
        };
        if produced == 0 {
            out.clear();
            return Ok(());
        }
    }
    if out.len() < 3 {
        out.clear();
    }
    Ok(())
}

/// Liang-Barsky parametric clip of segment `p0 -> p1`; returns `(t0, t1)`.
fn clip_segment(p0: [f32; 2], p1: [f32; 2], b: &Bounds) -> Option<(f32, f32)> {
    let dx = p1[0] - p0[0];
    let dy = p1[1] - p0[1];
    let mut t0 = 0.0f32;
    let mut t1 = 1.0f32;
    let tests = [
        (-dx, p0[0] - b.min_x),
        (dx, b.max_x - p0[0]),
        (-dy, p0[1] - b.min_y),
        (dy, b.max_y - p0[1]),
    ];
    for (p, q) in tests {
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
                if r > t0 {
                    t0 = r;
                }
            } else {
                if r < t0 {
                    return None;
                }
                if r < t1 {
                    t1 = r;
                }
            }
        }
    }
    // Zero-length overlap (grazing a corner) is not a segment.
    (t0 < t1).then_some((t0, t1))
}

fn point_at(p0: [f32; 2], p1: [f32; 2], t: f32, b: &Bounds) -> [f32; 2] {
    [
        (p0[0] + t * (p1[0] - p0[0])).clamp(b.min_x, b.max_x),
        (p0[1] + t * (p1[1] - p0[1])).clamp(b.min_y, b.max_y),
    ]
}

/// Clip a polyline to `bounds`. Appends clipped points to `out` and one entry
/// per contiguous part (its point count) to `parts`.
pub(crate) fn clip_polyline(
    input: &[[f32; 2]],
    bounds: &Bounds,
    out: &mut Vec<[f32; 2]>,
    parts: &mut Vec<u32>,
) {
    let mut current = 0u32;
    for pair in input.windows(2) {
        let (p0, p1) = (pair[0], pair[1]);
        let Some((t0, t1)) = clip_segment(p0, p1, bounds) else {
            close_part(&mut current, parts);
            continue;
        };
        let start_clipped = t0 > 0.0;
        let end_clipped = t1 < 1.0;
        if current > 0 && start_clipped {
            close_part(&mut current, parts);
        }
        if current == 0 {
            out.push(if start_clipped {
                point_at(p0, p1, t0, bounds)
            } else {
                p0
            });
            current = 1;
        }
        out.push(if end_clipped {
            point_at(p0, p1, t1, bounds)
        } else {
            p1
        });
        current += 1;
        if end_clipped {
            close_part(&mut current, parts);
        }
    }
    close_part(&mut current, parts);
}

fn close_part(current: &mut u32, parts: &mut Vec<u32>) {
    if *current > 0 {
        parts.push(*current);
        *current = 0;
    }
}

#[derive(Debug, Clone, Copy)]
struct Span {
    point_start: usize,
    point_len: usize,
    part_start: usize,
    part_len: usize,
}

/// Reusable output storage for CPU clipping of one [`Prepared`] batch.
#[derive(Debug, Default)]
pub(crate) struct CpuArena {
    points: Vec<[f32; 2]>,
    parts: Vec<u32>,
    spans: Vec<Span>,
    ring_out: Vec<[f32; 2]>,
    scratch: Vec<[f32; 2]>,
}

impl CpuArena {
    /// Clip every unit of `prepared` (no capacity limit).
    pub(crate) fn run(&mut self, prepared: &Prepared) {
        self.points.clear();
        self.parts.clear();
        self.spans.clear();
        for unit in &prepared.units {
            let input = &prepared.coords[unit.start as usize..(unit.start + unit.len) as usize];
            let point_start = self.points.len();
            let part_start = self.parts.len();
            match unit.kind {
                UnitKind::Ring => {
                    // `usize::MAX` capacity cannot overflow.
                    let _ = clip_ring(
                        input,
                        &unit.bounds,
                        usize::MAX,
                        &mut self.ring_out,
                        &mut self.scratch,
                    );
                    self.points.extend_from_slice(&self.ring_out);
                }
                UnitKind::Line => {
                    clip_polyline(input, &unit.bounds, &mut self.points, &mut self.parts);
                }
            }
            self.spans.push(Span {
                point_start,
                point_len: self.points.len() - point_start,
                part_start,
                part_len: self.parts.len() - part_start,
            });
        }
    }
}

impl UnitResults for CpuArena {
    fn unit(&self, index: usize) -> UnitView<'_> {
        let s = self.spans[index];
        UnitView {
            points: &self.points[s.point_start..s.point_start + s.point_len],
            parts: &self.parts[s.part_start..s.part_start + s.part_len],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const B: Bounds = Bounds {
        min_x: 0.0,
        min_y: 0.0,
        max_x: 10.0,
        max_y: 10.0,
    };

    fn ring(input: &[[f32; 2]], cap: usize) -> Result<Vec<[f32; 2]>, Overflow> {
        let (mut out, mut scratch) = (Vec::new(), Vec::new());
        clip_ring(input, &B, cap, &mut out, &mut scratch).map(|()| out)
    }

    fn area(r: &[[f32; 2]]) -> f32 {
        let mut a = 0.0;
        for i in 0..r.len() {
            let (p, q) = (r[i], r[(i + 1) % r.len()]);
            a += p[0] * q[1] - q[0] * p[1];
        }
        a / 2.0
    }

    #[test]
    fn ring_inside_is_unchanged() {
        let sq = [[1.0, 1.0], [2.0, 1.0], [2.0, 2.0], [1.0, 2.0]];
        assert_eq!(ring(&sq, usize::MAX).unwrap(), sq);
    }

    #[test]
    fn ring_outside_is_empty() {
        let sq = [[11.0, 1.0], [12.0, 1.0], [12.0, 2.0], [11.0, 2.0]];
        assert!(ring(&sq, usize::MAX).unwrap().is_empty());
    }

    #[test]
    fn ring_covering_box_becomes_box() {
        let big = [[-5.0, -5.0], [15.0, -5.0], [15.0, 15.0], [-5.0, 15.0]];
        let out = ring(&big, usize::MAX).unwrap();
        assert!((area(&out).abs() - 100.0).abs() < 1e-4, "{out:?}");
    }

    #[test]
    fn ring_overflow_is_reported() {
        let tri = [[-5.0, 5.0], [15.0, 2.0], [15.0, 8.0]];
        assert_eq!(ring(&tri, 3), Err(Overflow));
        assert!(ring(&tri, 64).is_ok());
    }

    #[test]
    fn line_leaving_and_reentering_yields_two_parts() {
        // Zig-zag: inside, out the right, back in, out the top.
        let line = [
            [2.0, 2.0],
            [12.0, 2.0],
            [12.0, 4.0],
            [4.0, 4.0],
            [4.0, 14.0],
        ];
        let (mut out, mut parts) = (Vec::new(), Vec::new());
        clip_polyline(&line, &B, &mut out, &mut parts);
        assert_eq!(parts, vec![2, 3]);
        assert_eq!(out[0], [2.0, 2.0]);
        assert_eq!(out[1], [10.0, 2.0]);
        assert_eq!(out[2], [10.0, 4.0]);
        assert_eq!(out[3], [4.0, 4.0]);
        assert_eq!(out[4], [4.0, 10.0]);
    }

    #[test]
    fn contiguous_inside_run_is_one_part() {
        let line = [[1.0, 1.0], [2.0, 5.0], [8.0, 5.0], [9.0, 9.0]];
        let (mut out, mut parts) = (Vec::new(), Vec::new());
        clip_polyline(&line, &B, &mut out, &mut parts);
        assert_eq!(parts, vec![4]);
        assert_eq!(out, line);
    }

    #[test]
    fn line_fully_outside_is_empty() {
        let line = [[-5.0, -5.0], [-1.0, 20.0]];
        let (mut out, mut parts) = (Vec::new(), Vec::new());
        clip_polyline(&line, &B, &mut out, &mut parts);
        assert!(out.is_empty() && parts.is_empty());
    }
}
