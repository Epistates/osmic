//! Projection and flattening of [`WorkItem`]s into clip *units*.
//!
//! A unit is the smallest independently clippable piece: one polygon ring or
//! one polyline. A GPU thread (or one CPU loop iteration) processes one unit.
//! Items map to units through a [`Plan`], which is also what reassembles
//! unit results into polygons-with-holes / multi-part lines.

use geo_types::{LineString, Polygon};
use osmic_core::geometry::Geometry;
use osmic_core::mercator::{lat_to_unit_y, lon_to_unit_x};

use crate::clip::{ClipOptions, MAX_ZOOM, WorkItem};
use crate::error::{AccelError, AccelResult};

/// Largest supported tile extent. 2^24 keeps tile-local integers exactly
/// representable in f32.
const MAX_EXTENT: u32 = 1 << 24;

/// Axis-aligned clip rectangle in tile-local space.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Bounds {
    pub min_x: f32,
    pub min_y: f32,
    pub max_x: f32,
    pub max_y: f32,
}

impl Bounds {
    fn contains(&self, p: [f32; 2]) -> bool {
        p[0] >= self.min_x && p[0] <= self.max_x && p[1] >= self.min_y && p[1] <= self.max_y
    }
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
}

struct TileProjection {
    n: f64,
    tx: f64,
    ty: f64,
    extent: f64,
}

impl TileProjection {
    /// Lon/lat degrees to tile-local coordinates in `[0, extent]` (before
    /// clipping). Mercator maths in f64, rounded to f32 once at the end.
    fn project(&self, lon: f64, lat: f64) -> AccelResult<[f32; 2]> {
        if !(lon.is_finite() && lat.is_finite()) {
            return Err(AccelError::InvalidInput(format!(
                "non-finite coordinate ({lon}, {lat})"
            )));
        }
        let mx = lon_to_unit_x(lon);
        let my = lat_to_unit_y(lat);
        Ok([
            ((mx * self.n - self.tx) * self.extent) as f32,
            ((my * self.n - self.ty) * self.extent) as f32,
        ])
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

        let proj = TileProjection {
            n: tiles_per_axis as f64,
            tx: f64::from(item.tile_x),
            ty: f64::from(item.tile_y),
            extent: f64::from(item.extent),
        };
        let extent = item.extent as f32;
        let lo = -(extent * options.buffer_fraction);
        let hi = extent * (1.0 + options.buffer_fraction);
        let bounds = Bounds {
            min_x: lo,
            min_y: lo,
            max_x: hi,
            max_y: hi,
        };

        let plan = match item.geometry {
            Geometry::Point(p) => {
                let projected = proj.project(p.x(), p.y())?;
                Plan::Points(
                    bounds
                        .contains(projected)
                        .then_some(projected)
                        .into_iter()
                        .collect(),
                )
            }
            Geometry::MultiPoint(mp) => {
                let mut inside = Vec::with_capacity(mp.0.len());
                for p in &mp.0 {
                    let projected = proj.project(p.x(), p.y())?;
                    if bounds.contains(projected) {
                        inside.push(projected);
                    }
                }
                Plan::Points(inside)
            }
            Geometry::Line(line) => {
                let first_unit = self.next_unit_index()?;
                let pushed = self.push_line(line, &proj, bounds)?;
                Plan::Lines {
                    first_unit,
                    count: u32::from(pushed),
                }
            }
            Geometry::MultiLine(lines) => {
                let first_unit = self.next_unit_index()?;
                let mut count = 0u32;
                for line in &lines.0 {
                    count += u32::from(self.push_line(line, &proj, bounds)?);
                }
                Plan::Lines { first_unit, count }
            }
            Geometry::Polygon(poly) => {
                let first_unit = self.next_unit_index()?;
                let mut ring_counts = Vec::with_capacity(1);
                self.push_polygon(poly, &proj, bounds, &mut ring_counts)?;
                Plan::Polygons {
                    first_unit,
                    ring_counts,
                }
            }
            Geometry::MultiPolygon(multi) => {
                let first_unit = self.next_unit_index()?;
                let mut ring_counts = Vec::with_capacity(multi.0.len());
                for poly in &multi.0 {
                    self.push_polygon(poly, &proj, bounds, &mut ring_counts)?;
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

    /// Returns whether a unit was pushed (lines with < 2 vertices are dropped).
    fn push_line(
        &mut self,
        line: &LineString<f64>,
        proj: &TileProjection,
        bounds: Bounds,
    ) -> AccelResult<bool> {
        if line.0.len() < 2 {
            return Ok(false);
        }
        let start = self.coords.len();
        for c in &line.0 {
            self.coords.push(proj.project(c.x, c.y)?);
        }
        self.push_unit(UnitKind::Line, start, bounds)?;
        Ok(true)
    }

    /// Pushes exterior + holes; records the number of rings pushed.
    /// Polygons whose exterior has fewer than 3 distinct-closure vertices are
    /// dropped entirely; degenerate holes are dropped individually.
    fn push_polygon(
        &mut self,
        poly: &Polygon<f64>,
        proj: &TileProjection,
        bounds: Bounds,
        ring_counts: &mut Vec<u32>,
    ) -> AccelResult<()> {
        let mark_coords = self.coords.len();
        let mark_units = self.units.len();
        if !self.push_ring(poly.exterior(), proj, bounds)? {
            self.coords.truncate(mark_coords);
            self.units.truncate(mark_units);
            return Ok(());
        }
        for hole in poly.interiors() {
            self.push_ring(hole, proj, bounds)?;
        }
        let rings = self.units.len() - mark_units;
        ring_counts.push(rings as u32);
        Ok(())
    }

    /// Returns whether a ring unit was pushed.
    fn push_ring(
        &mut self,
        ring: &LineString<f64>,
        proj: &TileProjection,
        bounds: Bounds,
    ) -> AccelResult<bool> {
        let mut verts = ring.0.as_slice();
        // geo rings repeat the first vertex at the end; GPU/CPU rings are open.
        if verts.len() >= 2 && verts.first() == verts.last() {
            verts = &verts[..verts.len() - 1];
        }
        if verts.len() < 3 {
            return Ok(false);
        }
        let start = self.coords.len();
        for c in verts {
            self.coords.push(proj.project(c.x, c.y)?);
        }
        self.push_unit(UnitKind::Ring, start, bounds)?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use geo_types::{Coord, MultiPolygon, polygon};

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
}
