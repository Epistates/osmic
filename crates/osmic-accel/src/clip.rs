//! Public clipping types and the CPU reference implementation.
//!
//! The CPU path and the GPU path share the same preparation step
//! ([`crate::prepare`]) and the same unit kernels' semantics
//! ([`crate::cpu`] mirrors `geometry_ops.metal`), so their results agree up to
//! f32 rounding.

use osmic_core::geometry::Geometry;

use crate::cpu::CpuArena;
use crate::error::{AccelError, AccelResult};
use crate::prepare::{Plan, Prepared};

/// One (geometry, tile) pair to clip.
///
/// The geometry is in lon/lat (EPSG:4326); it is projected to Web Mercator
/// tile-local coordinates in `[0, extent]` before clipping.
#[derive(Debug, Clone, Copy)]
pub struct WorkItem<'a> {
    /// Geometry in lon/lat degrees.
    pub geometry: &'a Geometry,
    /// Tile column.
    pub tile_x: u32,
    /// Tile row (XYZ scheme, origin top-left).
    pub tile_y: u32,
    /// Zoom level, `0..=MAX_ZOOM`.
    pub zoom: u8,
    /// Tile extent in integer units (typically 4096), `1..=2^24`.
    pub extent: u32,
}

/// Highest supported zoom level.
pub const MAX_ZOOM: u8 = 30;

/// Tunables shared by the CPU and GPU clippers.
///
/// Construct with [`ClipOptions::default`] and adjust fields; the struct is
/// `#[non_exhaustive]` so new options can be added without breaking callers.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct ClipOptions {
    /// Clip rectangle margin as a fraction of the tile extent, applied on all
    /// four sides (so the clip box is `[-f * extent, (1 + f) * extent]`).
    /// Must be finite and in `0.0..=1.0`. Default `0.05`.
    pub buffer_fraction: f32,
    /// GPU only. A polygon ring's per-ring output/scratch capacity is
    /// `ring_capacity_factor * vertices + ring_capacity_slack`. If a
    /// Sutherland-Hodgman stage would exceed it, the GPU kernel reports an
    /// overflow and the **whole item is recomputed on the CPU** (results are
    /// never truncated). Must be at least 1. Default `2`.
    pub ring_capacity_factor: u32,
    /// See [`ClipOptions::ring_capacity_factor`]. Default `16`.
    pub ring_capacity_slack: u32,
}

impl Default for ClipOptions {
    fn default() -> Self {
        Self {
            buffer_fraction: 0.05,
            ring_capacity_factor: 2,
            ring_capacity_slack: 16,
        }
    }
}

impl ClipOptions {
    pub(crate) fn validate(&self) -> AccelResult<()> {
        if !(self.buffer_fraction.is_finite() && (0.0..=1.0).contains(&self.buffer_fraction)) {
            return Err(AccelError::InvalidInput(format!(
                "buffer_fraction must be in 0.0..=1.0, got {}",
                self.buffer_fraction
            )));
        }
        if self.ring_capacity_factor == 0 {
            return Err(AccelError::InvalidInput(
                "ring_capacity_factor must be at least 1".into(),
            ));
        }
        Ok(())
    }

    /// Capacity (in vertices) of a polygon ring with `n` input vertices.
    #[cfg(osmic_metallib)]
    pub(crate) fn ring_capacity(&self, n: u32) -> u64 {
        u64::from(self.ring_capacity_factor) * u64::from(n) + u64::from(self.ring_capacity_slack)
    }
}

/// A clipped polygon: exterior ring plus interior rings (holes).
///
/// Rings are *open* (the closing vertex is not repeated) and keep the winding
/// of the input. Coordinates are tile-local f32.
#[derive(Debug, Clone, PartialEq)]
pub struct ClippedPolygon {
    /// Exterior ring, at least 3 vertices.
    pub exterior: Vec<[f32; 2]>,
    /// Interior rings, each at least 3 vertices.
    pub holes: Vec<Vec<[f32; 2]>>,
}

/// Result of clipping one [`WorkItem`], in tile-local f32 coordinates.
///
/// `#[non_exhaustive]`: multi-point results will be added alongside the
/// corresponding `osmic_core::Geometry` variants.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum ClippedGeometry {
    /// Nothing of the geometry intersects the clip box.
    Empty,
    /// A point inside the clip box.
    Point([f32; 2]),
    /// Several points inside the clip box (from a multi-point input).
    MultiPoint(Vec<[f32; 2]>),
    /// One or more polylines (each at least 2 vertices). Every contiguous run
    /// of the input line inside the clip box is its own part, so no
    /// connecting segments are invented where the line leaves and re-enters
    /// the box.
    Lines(Vec<Vec<[f32; 2]>>),
    /// One or more polygons with holes.
    Polygons(Vec<ClippedPolygon>),
}

impl ClippedGeometry {
    /// True if the result carries no geometry.
    pub fn is_empty(&self) -> bool {
        matches!(self, ClippedGeometry::Empty)
    }
}

/// Clip a batch on the CPU. This is the reference implementation the GPU path
/// is validated against; it is also the fallback on machines without Metal.
///
/// Results are returned in input order.
pub fn clip_batch_cpu(
    items: &[WorkItem<'_>],
    options: &ClipOptions,
) -> AccelResult<Vec<ClippedGeometry>> {
    options.validate()?;
    let mut prepared = Prepared::default();
    let mut arena = CpuArena::default();
    let mut out = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        prepared.clear();
        prepared
            .push_item(item, options)
            .map_err(|e| with_item_index(e, index))?;
        out.push(clip_prepared_item(&prepared, &mut arena));
    }
    Ok(out)
}

/// Clip a single item on the CPU (used for per-item GPU overflow fallback).
#[cfg(osmic_metallib)]
pub(crate) fn clip_item_cpu(
    item: &WorkItem<'_>,
    options: &ClipOptions,
) -> AccelResult<ClippedGeometry> {
    let mut prepared = Prepared::default();
    prepared.push_item(item, options)?;
    Ok(clip_prepared_item(&prepared, &mut CpuArena::default()))
}

fn clip_prepared_item(prepared: &Prepared, arena: &mut CpuArena) -> ClippedGeometry {
    arena.run(prepared);
    assemble(&prepared.plans[0], arena)
}

pub(crate) fn with_item_index(error: AccelError, index: usize) -> AccelError {
    match error {
        AccelError::InvalidInput(msg) => AccelError::InvalidInput(format!("item {index}: {msg}")),
        other => other,
    }
}

/// Read-only view of one clipped unit's output.
pub(crate) struct UnitView<'a> {
    pub points: &'a [[f32; 2]],
    /// Point count of each part (lines only; empty for rings).
    pub parts: &'a [u32],
}

/// Source of per-unit clip results (CPU arena or GPU buffers).
pub(crate) trait UnitResults {
    fn unit(&self, index: usize) -> UnitView<'_>;
}

/// Build the public result for one item from its unit results.
pub(crate) fn assemble(plan: &Plan, results: &impl UnitResults) -> ClippedGeometry {
    match plan {
        Plan::Points(points) => match points.as_slice() {
            [] => ClippedGeometry::Empty,
            [p] => ClippedGeometry::Point(*p),
            _ => ClippedGeometry::MultiPoint(points.clone()),
        },
        Plan::Lines { first_unit, count } => {
            let mut lines = Vec::new();
            for unit in *first_unit..*first_unit + *count {
                let view = results.unit(unit as usize);
                let mut start = 0usize;
                for &len in view.parts {
                    let end = start + len as usize;
                    lines.push(view.points[start..end].to_vec());
                    start = end;
                }
            }
            if lines.is_empty() {
                ClippedGeometry::Empty
            } else {
                ClippedGeometry::Lines(lines)
            }
        }
        Plan::Polygons {
            first_unit,
            ring_counts,
        } => {
            let mut polygons = Vec::new();
            let mut unit = *first_unit as usize;
            for &rings in ring_counts {
                let exterior = results.unit(unit).points;
                if exterior.len() >= 3 {
                    let holes = (1..rings as usize)
                        .map(|r| results.unit(unit + r).points)
                        .filter(|ring| ring.len() >= 3)
                        .map(<[[f32; 2]]>::to_vec)
                        .collect();
                    polygons.push(ClippedPolygon {
                        exterior: exterior.to_vec(),
                        holes,
                    });
                }
                unit += rings as usize;
            }
            if polygons.is_empty() {
                ClippedGeometry::Empty
            } else {
                ClippedGeometry::Polygons(polygons)
            }
        }
    }
}
