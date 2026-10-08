//! Ramer–Douglas–Peucker line simplification.
//!
//! Same result as `geo`'s `Simplify` (point-to-segment distances, a vertex
//! is kept when its distance exceeds the tolerance, the last of equally
//! distant vertices splits), computed iteratively on squared distances
//! without allocating per recursion level.

use geo_types::Coord;

/// Simplify `points` with tolerance `epsilon`, appending the kept vertices
/// to `out`. The first and last vertices are always kept; with
/// `epsilon <= 0` (or fewer than three vertices) everything is kept.
pub fn rdp(points: &[Coord<f64>], epsilon: f64, out: &mut Vec<Coord<f64>>) {
    let n = points.len();
    if n < 3 || epsilon <= 0.0 || epsilon.is_nan() {
        out.extend_from_slice(points);
        return;
    }
    let eps2 = epsilon * epsilon;
    let mut keep = vec![false; n];
    keep[0] = true;
    keep[n - 1] = true;
    let mut stack = vec![(0usize, n - 1)];
    while let Some((a, b)) = stack.pop() {
        if b - a < 2 {
            continue;
        }
        let (start, end) = (points[a], points[b]);
        let (mut farthest, mut farthest_d2) = (a + 1, 0.0);
        for (i, &p) in points.iter().enumerate().take(b).skip(a + 1) {
            let d2 = segment_distance2(p, start, end);
            if d2 >= farthest_d2 {
                farthest = i;
                farthest_d2 = d2;
            }
        }
        if farthest_d2 > eps2 {
            keep[farthest] = true;
            stack.push((a, farthest));
            stack.push((farthest, b));
        }
    }
    out.extend(
        points
            .iter()
            .zip(&keep)
            .filter_map(|(&p, &k)| k.then_some(p)),
    );
}

/// Squared distance from `p` to the segment `a`–`b`.
fn segment_distance2(p: Coord<f64>, a: Coord<f64>, b: Coord<f64>) -> f64 {
    let (dx, dy) = (b.x - a.x, b.y - a.y);
    let len2 = dx * dx + dy * dy;
    let dist2 = |q: Coord<f64>| (p.x - q.x) * (p.x - q.x) + (p.y - q.y) * (p.y - q.y);
    if len2 == 0.0 {
        return dist2(a);
    }
    let r = ((p.x - a.x) * dx + (p.y - a.y) * dy) / len2;
    if r <= 0.0 {
        dist2(a)
    } else if r >= 1.0 {
        dist2(b)
    } else {
        let s = ((a.y - p.y) * dx - (a.x - p.x) * dy) / len2;
        s * s * len2
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(x: f64, y: f64) -> Coord<f64> {
        Coord { x, y }
    }

    fn simplify(points: &[Coord<f64>], epsilon: f64) -> Vec<Coord<f64>> {
        let mut out = Vec::new();
        rdp(points, epsilon, &mut out);
        out
    }

    #[test]
    fn matches_the_classic_example() {
        // geo's documented example.
        let line = [
            c(0.0, 0.0),
            c(5.0, 4.0),
            c(11.0, 5.5),
            c(17.3, 3.2),
            c(27.8, 0.1),
        ];
        assert_eq!(
            simplify(&line, 1.0),
            [c(0.0, 0.0), c(5.0, 4.0), c(11.0, 5.5), c(27.8, 0.1)]
        );
    }

    #[test]
    fn keeps_endpoints_and_handles_degenerate_input() {
        assert!(simplify(&[], 1.0).is_empty());
        assert_eq!(simplify(&[c(1.0, 1.0)], 1.0), [c(1.0, 1.0)]);
        let flat = [c(0.0, 0.0), c(1.0, 0.0), c(2.0, 0.0), c(3.0, 0.0)];
        assert_eq!(simplify(&flat, 0.1), [c(0.0, 0.0), c(3.0, 0.0)]);
        assert_eq!(simplify(&flat, 0.0), flat, "zero tolerance keeps all");
        // A closed ring (distances fall back to the shared endpoint) loses
        // only its collinear midpoint.
        let ring = [
            c(0.0, 0.0),
            c(5.0, 0.0),
            c(10.0, 0.0),
            c(10.0, 10.0),
            c(0.0, 10.0),
            c(0.0, 0.0),
        ];
        assert_eq!(
            simplify(&ring, 1.0),
            [c(0.0, 0.0), c(10.0, 0.0), c(10.0, 10.0), c(0.0, 10.0), c(0.0, 0.0)]
        );
    }

    #[test]
    fn agrees_with_geo_on_random_lines() {
        use geo::Simplify;
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 11) as f64 / (1u64 << 53) as f64
        };
        for len in [3, 10, 100, 1000] {
            for eps in [0.001, 0.01, 0.1] {
                let line: Vec<Coord<f64>> = (0..len)
                    .map(|i| c(i as f64 / len as f64, next() * 0.2))
                    .collect();
                let expected = geo_types::LineString(line.clone()).simplify(eps).0;
                assert_eq!(simplify(&line, eps), expected, "len {len} eps {eps}");
            }
        }
    }
}
