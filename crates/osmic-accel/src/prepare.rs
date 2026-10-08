//! Projection and flattening of [`WorkItem`]s into clip *units*.
//!
//! A unit is the smallest independently clippable piece: one polygon ring or
//! one polyline. A GPU thread (or one CPU loop iteration) processes one unit.
//! Items map to units through a [`Plan`], which is also what reassembles
//! unit results into polygons-with-holes / multi-part lines.

use geo_types::{Coord, LineString, Polygon};
use osmic_core::geometry::Geometry;
use osmic_core::mercator::{lat_to_unit_y, lon_to_unit_x};

use crate::clip::{ClipOptions, MAX_ZOOM, WorkItem};
use crate::cpu;
use crate::error::{AccelError, AccelResult};

/// Largest supported tile extent. 2^24 keeps tile-local integers exactly
/// representable in f32.
const MAX_EXTENT: u32 = 1 << 24;

/// Margin of the guard box around the clip box, in tile extents.
///
/// Coordinates are projected in f64 and only rounded to f32 for clipping.
/// A vertex far from the tile (a huge but finite longitude, or the far end
/// of an edge spanning thousands of tiles) does not survive that rounding:
/// it saturates to infinity, or the edge loses most of its precision. So
/// geometry reaching beyond the guard box is first clipped to it in f64.
/// The guard box contains the clip box, so this changes where long edges
/// are cut, not the final result, and the f32 stage (and the GPU) only ever
/// sees coordinates within a few extents of the tile.
const GUARD_MARGIN: f64 = 1.0;

/// Axis-aligned clip rectangle in tile-local space.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Bounds<T = f32> {
    pub min_x: T,
    pub min_y: T,
    pub max_x: T,
    pub max_y: T,
}

impl<T: PartialOrd> Bounds<T> {
    pub(crate) fn contains(&self, p: [T; 2]) -> bool {
        p[0] >= self.min_x && p[0] <= self.max_x && p[1] >= self.min_y && p[1] <= self.max_y
    }
}

impl Bounds {
    /// The guard box (f64) around this clip box.
    fn guard(&self, extent: f64) -> Bounds<f64> {
        let margin = extent * GUARD_MARGIN;
        Bounds {
            min_x: f64::from(self.min_x) - margin,
            min_y: f64::from(self.min_y) - margin,
            max_x: f64::from(self.max_x) + margin,
            max_y: f64::from(self.max_y) + margin,
        }
    }
}

/// Round a tile-local coordinate inside a guard box to f32. The guard box
/// spans a few tile extents (at most 2^24 each), so the result is finite.
fn narrow(p: [f64; 2]) -> [f32; 2] {
    [p[0] as f32, p[1] as f32]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnitKind {
    /// Open polyline; clipped into zero or more parts.
    Line,
    /// Closed polygon ring (closing vertex not repeated); clipped into at
    /// most one ring.
    Ring,
}

/// One clippable piece, referencing a range of [`Prepared::coords`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct Unit {
    pub kind: UnitKind,
    pub start: u32,
    pub len: u32,
    pub bounds: Bounds,
}

/// How an item's results are assembled from its units.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Plan {
    /// Points inside the clip box, tested on the host (a GPU round-trip
    /// for a containment test would cost far more than it saves).
    Points(Vec<[f32; 2]>),
    /// `count` consecutive line units starting at `first_unit`.
    Lines { first_unit: u32, count: u32 },
    /// Consecutive ring units; `ring_counts[i]` rings (exterior first) per
    /// polygon.
    Polygons {
        first_unit: u32,
        ring_counts: Vec<u32>,
    },
}

impl Plan {
    /// Range of unit indices this item owns.
    #[cfg(osmic_metallib)]
    pub(crate) fn unit_range(&self) -> std::ops::Range<usize> {
        match self {
            Plan::Points(_) => 0..0,
            Plan::Lines { first_unit, count } => {
                *first_unit as usize..(*first_unit + *count) as usize
            }
            Plan::Polygons {
                first_unit,
                ring_counts,
            } => {
                let total: u32 = ring_counts.iter().sum();
                *first_unit as usize..(*first_unit + total) as usize
            }
        }
    }
}

/// Flattened, projected batch.
#[derive(Debug, Default)]
pub(crate) struct Prepared {
    /// Tile-local coordinates of all units, back to back.
    pub coords: Vec<[f32; 2]>,
    pub units: Vec<Unit>,
    /// One plan per pushed item, in push order.
    pub plans: Vec<Plan>,
    /// Reusable f64 buffers for projecting and guard-clipping one unit.
    wide: Wide,
}

#[derive(Debug, Default)]
struct Wide {
    input: Vec<[f64; 2]>,
    out: Vec<[f64; 2]>,
    scratch: Vec<[f64; 2]>,
    parts: Vec<u32>,
}

struct TileProjection {
    n: f64,
    tx: f64,
    ty: f64,
    extent: f64,
}

impl TileProjection {
    /// Lon/lat degrees to tile-local coordinates (`[0, extent]` inside the
    /// tile), in f64. Rejects non-finite input. The result may be infinite
    /// (a huge longitude overflowing even f64) but is never NaN: latitude
    /// is clamped, and an infinite x stays infinite through the affine map.
    fn project_finite_input(&self, lon: f64, lat: f64) -> AccelResult<[f64; 2]> {
        if !(lon.is_finite() && lat.is_finite()) {
            return Err(AccelError::InvalidInput(format!(
                "non-finite coordinate ({lon}, {lat})"
            )));
        }
        let mx = lon_to_unit_x(lon);
        let my = lat_to_unit_y(lat);
        Ok([
            (mx * self.n - self.tx) * self.extent,
            (my * self.n - self.ty) * self.extent,
        ])
    }

    /// [`Self::project_finite_input`], also rejecting results that overflow.
    fn project(&self, lon: f64, lat: f64) -> AccelResult<[f64; 2]> {
        let p = self.project_finite_input(lon, lat)?;
        if !(p[0].is_finite() && p[1].is_finite()) {
            return Err(AccelError::InvalidInput(format!(
                "coordinate ({lon}, {lat}) is too far from the tile to project"
            )));
        }
        Ok(p)
    }
}

impl Prepared {
    pub(crate) fn clear(&mut self) {
        self.coords.clear();
        self.units.clear();
        self.plans.clear();
    }

    /// Project `item` and append its units and plan.
    ///
    /// On error the builder is left in an unspecified state; discard it.
    pub(crate) fn push_item(
        &mut self,
        item: &WorkItem<'_>,
        options: &ClipOptions,
    ) -> AccelResult<()> {
        if item.zoom > MAX_ZOOM {
            return Err(AccelError::InvalidInput(format!(
                "zoom {} exceeds the maximum of {MAX_ZOOM}",
                item.zoom
            )));
        }
        let tiles_per_axis = 1u64 << item.zoom;
        if u64::from(item.tile_x) >= tiles_per_axis || u64::from(item.tile_y) >= tiles_per_axis {
            return Err(AccelError::InvalidInput(format!(
                "tile {}/{} out of range for zoom {}",
                item.tile_x, item.tile_y, item.zoom
            )));
        }
        if item.extent == 0 || item.extent > MAX_EXTENT {
            return Err(AccelError::InvalidInput(format!(
                "extent {} must be in 1..={MAX_EXTENT}",
                item.extent
            )));
        }

        let extent = item.extent as f32;
        let lo = -(extent * options.buffer_fraction);
        let hi = extent * (1.0 + options.buffer_fraction);
        let bounds = Bounds {
            min_x: lo,
            min_y: lo,
            max_x: hi,
            max_y: hi,
        };
        let frame = ItemFrame {
            proj: TileProjection {
                n: tiles_per_axis as f64,
                tx: f64::from(item.tile_x),
                ty: f64::from(item.tile_y),
                extent: f64::from(item.extent),
            },
            bounds,
            guard: bounds.guard(f64::from(item.extent)),
        };

        let plan = match item.geometry {
            Geometry::Point(p) => Plan::Points(frame.point(p.x(), p.y())?.into_iter().collect()),
            Geometry::MultiPoint(mp) => {
                let mut inside = Vec::with_capacity(mp.0.len());
                for p in &mp.0 {
                    inside.extend(frame.point(p.x(), p.y())?);
                }
                Plan::Points(inside)
            }
            Geometry::Line(line) => {
                let first_unit = self.next_unit_index()?;
                let count = self.push_line(line, &frame)?;
                Plan::Lines { first_unit, count }
            }
            Geometry::MultiLine(lines) => {
                let first_unit = self.next_unit_index()?;
                let mut count = 0u32;
                for line in &lines.0 {
                    count += self.push_line(line, &frame)?;
                }
                Plan::Lines { first_unit, count }
            }
            Geometry::Polygon(poly) => {
                let first_unit = self.next_unit_index()?;
                let mut ring_counts = Vec::with_capacity(1);
                self.push_polygon(poly, &frame, &mut ring_counts)?;
                Plan::Polygons {
                    first_unit,
                    ring_counts,
                }
            }
            Geometry::MultiPolygon(multi) => {
                let first_unit = self.next_unit_index()?;
                let mut ring_counts = Vec::with_capacity(multi.0.len());
                for poly in &multi.0 {
                    self.push_polygon(poly, &frame, &mut ring_counts)?;
                }
                Plan::Polygons {
                    first_unit,
                    ring_counts,
                }
            }
        };
        self.plans.push(plan);
        Ok(())
    }

    fn next_unit_index(&self) -> AccelResult<u32> {
        u32::try_from(self.units.len())
            .map_err(|_| AccelError::InvalidInput("too many clip units in one batch".into()))
    }

    fn push_unit(&mut self, kind: UnitKind, start: usize, bounds: Bounds) -> AccelResult<()> {
        let len = self.coords.len() - start;
        let (start, len) = match (u32::try_from(start), u32::try_from(len)) {
            (Ok(s), Ok(l)) => (s, l),
            _ => {
                return Err(AccelError::InvalidInput(
                    "too many vertices in one batch".into(),
                ));
            }
        };
        self.units.push(Unit {
            kind,
            start,
            len,
            bounds,
        });
        Ok(())
    }

    /// Narrow `points` (inside the guard box) to f32 and push them as one
    /// unit.
    fn push_narrowed(
        &mut self,
        kind: UnitKind,
        points: &[[f64; 2]],
        bounds: Bounds,
    ) -> AccelResult<()> {
        let start = self.coords.len();
        self.coords.extend(points.iter().copied().map(narrow));
        self.push_unit(kind, start, bounds)
    }

    /// Project `verts`. In the common case, all of them inside the guard
    /// box, they are pushed (as f32) as one unit and this returns `true`.
    /// Otherwise nothing is pushed, the f64 projections are left in
    /// `self.wide.input` for the caller to clip to the guard box, and this
    /// returns `false`.
    fn push_projected(
        &mut self,
        kind: UnitKind,
        verts: &[Coord<f64>],
        frame: &ItemFrame,
    ) -> AccelResult<bool> {
        let start = self.coords.len();
        let wide = &mut self.wide.input;
        wide.clear();
        // Track the bounding box instead of testing every vertex: branch-free
        // in the loop, and an infinite coordinate (never NaN) still lands
        // outside the guard box.
        let (mut lo, mut hi) = ([f64::INFINITY; 2], [f64::NEG_INFINITY; 2]);
        for c in verts {
            let p = frame.proj.project_finite_input(c.x, c.y)?;
            lo = [lo[0].min(p[0]), lo[1].min(p[1])];
            hi = [hi[0].max(p[0]), hi[1].max(p[1])];
            wide.push(p);
            self.coords.push(narrow(p));
        }
        if frame.guard.contains(lo) && frame.guard.contains(hi) {
            self.push_unit(kind, start, frame.bounds)?;
            return Ok(true);
        }
        self.coords.truncate(start);
        if !lo.iter().chain(&hi).all(|v| v.is_finite()) {
            return Err(AccelError::InvalidInput(
                "a coordinate is too far from the tile to project".into(),
            ));
        }
        Ok(false)
    }

    /// Returns the number of units pushed: none for lines with < 2 vertices
    /// or entirely outside the guard box, several when the line leaves the
    /// guard box and comes back.
    fn push_line(&mut self, line: &LineString<f64>, frame: &ItemFrame) -> AccelResult<u32> {
        if line.0.len() < 2 {
            return Ok(0);
        }
        if self.push_projected(UnitKind::Line, &line.0, frame)? {
            return Ok(1);
        }
        // Each run inside the guard box becomes its own unit: joining them
        // would invent a segment that may cross the tile.
        let mut wide = std::mem::take(&mut self.wide);
        wide.out.clear();
        wide.parts.clear();
        cpu::clip_polyline(&wide.input, &frame.guard, &mut wide.out, &mut wide.parts);
        let mut from = 0;
        for &len in &wide.parts {
            let to = from + len as usize;
            self.push_narrowed(UnitKind::Line, &wide.out[from..to], frame.bounds)?;
            from = to;
        }
        let pushed = wide.parts.len() as u32;
        self.wide = wide;
        Ok(pushed)
    }

    /// Pushes exterior + holes; records the number of rings pushed.
    /// Polygons whose exterior has fewer than 3 distinct-closure vertices
    /// (or lies outside the guard box) are dropped entirely; such holes are
    /// dropped individually.
    fn push_polygon(
        &mut self,
        poly: &Polygon<f64>,
        frame: &ItemFrame,
        ring_counts: &mut Vec<u32>,
    ) -> AccelResult<()> {
        let mark_coords = self.coords.len();
        let mark_units = self.units.len();
        if !self.push_ring(poly.exterior(), frame)? {
            self.coords.truncate(mark_coords);
            self.units.truncate(mark_units);
            return Ok(());
        }
        for hole in poly.interiors() {
            self.push_ring(hole, frame)?;
        }
        let rings = self.units.len() - mark_units;
        ring_counts.push(rings as u32);
        Ok(())
    }

    /// Returns whether a ring unit was pushed.
    fn push_ring(&mut self, ring: &LineString<f64>, frame: &ItemFrame) -> AccelResult<bool> {
        let mut verts = ring.0.as_slice();
        // geo rings repeat the first vertex at the end; GPU/CPU rings are open.
        if verts.len() >= 2 && verts.first() == verts.last() {
            verts = &verts[..verts.len() - 1];
        }
        if verts.len() < 3 {
            return Ok(false);
        }
        if self.push_projected(UnitKind::Ring, verts, frame)? {
            return Ok(true);
        }
        let mut wide = std::mem::take(&mut self.wide);
        // Unlimited capacity cannot overflow; fewer than 3 vertices come
        // back as an empty ring.
        let _ = cpu::clip_ring(
            &wide.input,
            &frame.guard,
            usize::MAX,
            &mut wide.out,
            &mut wide.scratch,
        );
        let keep = !wide.out.is_empty();
        if keep {
            self.push_narrowed(UnitKind::Ring, &wide.out, frame.bounds)?;
        }
        self.wide = wide;
        Ok(keep)
    }
}

/// Where one item's geometry goes: its tile projection, clip box and guard
/// box.
struct ItemFrame {
    proj: TileProjection,
    bounds: Bounds,
    guard: Bounds<f64>,
}

impl ItemFrame {
    /// A point's tile-local position if it lies inside the clip box.
    fn point(&self, lon: f64, lat: f64) -> AccelResult<Option<[f32; 2]>> {
        let p = self.proj.project(lon, lat)?;
        // The f32 test decides, as for every other geometry; the guard test
        // only keeps far-away points from being rounded at all.
        Ok(Some(p)
            .filter(|&p| self.guard.contains(p))
            .map(narrow)
            .filter(|&p| self.bounds.contains(p)))
    }
}

#[cfg(test)]
mod tests {
    use geo_types::{MultiPolygon, polygon};

    use super::*;

    fn item(geometry: &Geometry) -> WorkItem<'_> {
        WorkItem {
            geometry,
            tile_x: 0,
            tile_y: 0,
            zoom: 0,
            extent: 4096,
        }
    }

    #[test]
    fn multipolygon_keeps_all_polygons_and_holes() {
        let a = polygon!(
            exterior: [(x: -10.0, y: -10.0), (x: 10.0, y: -10.0), (x: 10.0, y: 10.0), (x: -10.0, y: 10.0)],
            interiors: [[(x: -2.0, y: -2.0), (x: 2.0, y: -2.0), (x: 2.0, y: 2.0), (x: -2.0, y: 2.0)]],
        );
        let b = polygon![(x: 20.0, y: 20.0), (x: 30.0, y: 20.0), (x: 30.0, y: 30.0)];
        let geom = Geometry::MultiPolygon(MultiPolygon(vec![a, b]));
        let mut prep = Prepared::default();
        prep.push_item(&item(&geom), &ClipOptions::default())
            .unwrap();
        assert_eq!(prep.units.len(), 3);
        assert_eq!(
            prep.plans[0],
            Plan::Polygons {
                first_unit: 0,
                ring_counts: vec![2, 1]
            }
        );
        #[cfg(osmic_metallib)]
        assert_eq!(prep.plans[0].unit_range(), 0..3);
        // Closing vertex stripped.
        assert_eq!(prep.units[0].len, 4);
    }

    #[test]
    fn degenerate_inputs_are_dropped_not_errors() {
        let geom = Geometry::Line(LineString(vec![Coord { x: 1.0, y: 1.0 }]));
        let mut prep = Prepared::default();
        prep.push_item(&item(&geom), &ClipOptions::default())
            .unwrap();
        assert_eq!(
            prep.plans[0],
            Plan::Lines {
                first_unit: 0,
                count: 0
            }
        );
    }

    #[test]
    fn invalid_items_are_rejected() {
        let geom = Geometry::Point(geo_types::Point::new(f64::NAN, 0.0));
        let mut prep = Prepared::default();
        assert!(matches!(
            prep.push_item(&item(&geom), &ClipOptions::default()),
            Err(AccelError::InvalidInput(_))
        ));
        let ok = Geometry::Point(geo_types::Point::new(0.0, 0.0));
        let mut bad = item(&ok);
        bad.zoom = 64;
        assert!(prep.push_item(&bad, &ClipOptions::default()).is_err());
        let mut bad = item(&ok);
        bad.tile_x = 1;
        assert!(prep.push_item(&bad, &ClipOptions::default()).is_err());
        let mut bad = item(&ok);
        bad.extent = 0;
        assert!(prep.push_item(&bad, &ClipOptions::default()).is_err());
    }

    #[test]
    fn far_geometry_is_cut_to_the_guard_box_before_rounding() {
        // At z0 the guard box is the clip box plus one extent: lon ±180
        // spans the tile, the guard box ends near lon 558, so lon 800
        // lies well outside it.
        let line = Geometry::Line(LineString::from(vec![
            (0.0, 0.0),
            (800.0, 0.0),
            (800.0, 10.0),
            (10.0, 10.0),
        ]));
        let mut prep = Prepared::default();
        prep.push_item(&item(&line), &ClipOptions::default())
            .unwrap();
        assert_eq!(
            prep.plans[0],
            Plan::Lines {
                first_unit: 0,
                count: 2
            },
            "leaving the guard box and coming back gives two runs"
        );
        let guard_hi = 4096.0 * 1.05 + 4096.0;
        assert!(
            prep.coords.iter().flatten().all(|v| v.abs() <= guard_hi),
            "{:?}",
            prep.coords
        );

        let ring = Geometry::Polygon(polygon![
            (x: 0.0, y: 0.0), (x: 1e300, y: 0.0), (x: 0.0, y: 10.0),
        ]);
        let mut prep = Prepared::default();
        prep.push_item(&item(&ring), &ClipOptions::default())
            .unwrap();
        assert_eq!(prep.units.len(), 1);
        assert!(
            prep.coords
                .iter()
                .flatten()
                .all(|v| v.is_finite() && v.abs() <= guard_hi),
            "{:?}",
            prep.coords
        );
    }
}
