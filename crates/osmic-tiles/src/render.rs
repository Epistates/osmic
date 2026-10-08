//! Feature → per-zoom, per-tile geometry.
//!
//! For each feature the renderer:
//!
//! 1. projects its WGS84 coordinates once into the unit Web Mercator square;
//! 2. walks zoom levels from `max_zoom` down to the feature's minimum zoom,
//!    simplifying with a tolerance expressed in screen pixels (each zoom
//!    starts from the previous, more detailed, result) and stopping once
//!    the feature is smaller than `min_size_px`;
//! 3. slices the geometry into tiles by recursive bisection of the tile
//!    range — each level clips to a band along one axis, so cost grows with
//!    vertices × log(tiles) rather than vertices × tiles — with every tile
//!    widened by `buffer_px` so features near an edge also reach the
//!    neighbouring tile's buffer;
//! 4. quantises to tile-local integers, removes degenerate rings and
//!    enforces MVT 2.1 winding (exteriors positive, holes negative).
//!
//! Boundaries are emitted as their outlines (lines), the representation
//! map styles draw; filled boundary polygons would add a feature to every
//! tile they cover.

use geo_types::{Coord, LineString};

use osmic_core::clip::{Axis, clip_line_band, clip_ring_band, ring_signed_area_2x};
use osmic_core::mercator::{lat_to_unit_y, lon_to_unit_x, tiles_per_axis};
use osmic_core::simplify::rdp;
use osmic_core::{Geometry, TileCoord, Zoom};
use osmic_osm::{Feature, Layer, TagStore};

use crate::model::{GeomType, TileFeature, ring_area2};

/// Which attributes are written into tiles.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum AttributeMode {
    /// `class` plus names, references and the address/contact keys of
    /// [`osmic_osm::tags::CURATED_KEYS`].
    #[default]
    Curated,
    /// `class` plus every tag kept on the feature.
    All,
}

/// Rendering parameters.
///
/// Start from [`RenderConfig::default`] and set the fields to change; new
/// fields may be added in minor releases.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct RenderConfig {
    /// Lowest zoom level to render.
    pub min_zoom: u8,
    /// Highest zoom level to render (at most [`Zoom::MAX`]).
    pub max_zoom: u8,
    /// Tile coordinate extent (MVT default 4096).
    pub extent: u32,
    /// Extra margin rendered around each tile, in 256-px screen pixels.
    pub buffer_px: f64,
    /// Douglas–Peucker tolerance in screen pixels below `max_zoom`.
    pub simplify_px: f64,
    /// Tolerance at `max_zoom` (usually finer; geometry there may be
    /// over-zoomed by clients).
    pub simplify_px_max_zoom: f64,
    /// Lines shorter than this and polygons with less area than its square
    /// (screen pixels) are dropped below `max_zoom`.
    pub min_size_px: f64,
    /// Which attributes are written.
    pub attributes: AttributeMode,
}

impl Default for RenderConfig {
    fn default() -> Self {
        Self {
            min_zoom: 0,
            max_zoom: 14,
            extent: 4096,
            buffer_px: 4.0,
            simplify_px: 0.1,
            simplify_px_max_zoom: 0.0625,
            min_size_px: 1.0,
            attributes: AttributeMode::Curated,
        }
    }
}

/// One feature rendered into one tile.
///
/// Every piece of a feature borrows the same attribute list, so a caller can
/// process the attributes once per feature instead of once per tile.
#[derive(Debug, Clone, PartialEq)]
pub struct RenderedFeature<'a> {
    /// The tile this piece belongs to.
    pub tile: TileCoord,
    /// The feature's layer.
    pub layer: Layer,
    /// [`osmic_osm::FeatureKind::importance`]; higher is more important.
    pub importance: u8,
    /// Logarithmic size class at this zoom (larger = bigger feature).
    pub size_class: u16,
    /// Vector-tile feature id.
    pub id: Option<u64>,
    /// Geometry type of `parts`.
    pub geom_type: GeomType,
    /// Tile-local geometry, laid out as in [`TileFeature::parts`].
    pub parts: Vec<Vec<[i32; 2]>>,
    /// The feature's attributes, shared by all of its pieces.
    pub attributes: &'a [(String, String)],
}

impl RenderedFeature<'_> {
    /// The piece as a self-contained [`TileFeature`] (copies the
    /// attributes).
    pub fn into_tile_feature(self) -> TileFeature {
        TileFeature {
            id: self.id,
            geom_type: self.geom_type,
            parts: self.parts,
            attributes: self.attributes.to_vec(),
        }
    }
}

type Ring = Vec<Coord<f64>>;

/// Geometry in a planar space (unit square or world tile units).
#[derive(Debug, Clone)]
enum Shape {
    Points(Vec<Coord<f64>>),
    Lines(Vec<Vec<Coord<f64>>>),
    /// Polygons as rings; first ring of each is the exterior. Rings are
    /// closed (first == last).
    Polygons(Vec<Vec<Ring>>),
}

impl Shape {
    fn is_empty(&self) -> bool {
        match self {
            Self::Points(p) => p.is_empty(),
            Self::Lines(l) => l.is_empty(),
            Self::Polygons(p) => p.is_empty(),
        }
    }

    fn map(&self, f: impl Fn(Coord<f64>) -> Coord<f64>) -> Self {
        let ring = |r: &Ring| r.iter().map(|&c| f(c)).collect::<Ring>();
        match self {
            Self::Points(p) => Self::Points(p.iter().map(|&c| f(c)).collect()),
            Self::Lines(l) => Self::Lines(l.iter().map(ring).collect()),
            Self::Polygons(p) => {
                Self::Polygons(p.iter().map(|rs| rs.iter().map(ring).collect()).collect())
            }
        }
    }

    fn bbox(&self) -> Option<(Coord<f64>, Coord<f64>)> {
        let mut it: Box<dyn Iterator<Item = &Coord<f64>>> = match self {
            Self::Points(p) => Box::new(p.iter()),
            Self::Lines(l) => Box::new(l.iter().flatten()),
            // Exteriors bound their holes.
            Self::Polygons(p) => Box::new(p.iter().filter_map(|rs| rs.first()).flatten()),
        };
        let first = *it.next()?;
        Some(it.fold((first, first), |(lo, hi), c| {
            (
                Coord {
                    x: lo.x.min(c.x),
                    y: lo.y.min(c.y),
                },
                Coord {
                    x: hi.x.max(c.x),
                    y: hi.y.max(c.y),
                },
            )
        }))
    }

    /// Length (lines) or area (polygons) in this space's units.
    fn size(&self) -> f64 {
        match self {
            Self::Points(_) => 0.0,
            Self::Lines(l) => l
                .iter()
                .map(|p| {
                    p.windows(2)
                        .map(|w| {
                            let (dx, dy) = (w[1].x - w[0].x, w[1].y - w[0].y);
                            (dx * dx + dy * dy).sqrt()
                        })
                        .sum::<f64>()
                })
                .sum(),
            Self::Polygons(p) => p
                .iter()
                .map(|rings| {
                    let mut area = 0.0;
                    for (i, r) in rings.iter().enumerate() {
                        let a = ring_signed_area_2x(&r[..r.len().saturating_sub(1)]).abs() / 2.0;
                        area += if i == 0 { a } else { -a };
                    }
                    area.max(0.0)
                })
                .sum(),
        }
    }

    fn simplify(&self, tolerance: f64) -> Self {
        let simplify = |r: &Ring| {
            let mut out = Vec::with_capacity(r.len());
            rdp(r, tolerance, &mut out);
            out
        };
        match self {
            Self::Points(_) => self.clone(),
            Self::Lines(l) => {
                Self::Lines(l.iter().map(simplify).filter(|p| p.len() >= 2).collect())
            }
            Self::Polygons(p) => Self::Polygons(
                p.iter()
                    .filter_map(|rings| {
                        let mut out: Vec<Ring> = Vec::with_capacity(rings.len());
                        for (i, r) in rings.iter().enumerate() {
                            let s = simplify(r);
                            if s.len() >= 4 {
                                out.push(s);
                            } else if i == 0 {
                                return None; // exterior collapsed
                            }
                        }
                        Some(out)
                    })
                    .collect(),
            ),
        }
    }

    /// Clip to `min <= axis <= max`.
    fn clip(&self, axis: Axis, min: f64, max: f64) -> Self {
        let get = |c: &Coord<f64>| if axis == Axis::X { c.x } else { c.y };
        match self {
            Self::Points(p) => Self::Points(
                p.iter()
                    .filter(|c| (min..=max).contains(&get(c)))
                    .copied()
                    .collect(),
            ),
            Self::Lines(l) => {
                let mut out = Vec::new();
                for part in l {
                    clip_line_band(part, axis, min, max, &mut out);
                }
                Self::Lines(out)
            }
            Self::Polygons(p) => Self::Polygons(
                p.iter()
                    .filter_map(|rings| {
                        let ext = clip_ring_band(rings.first()?, axis, min, max);
                        if ext.is_empty() {
                            return None;
                        }
                        let mut out = vec![ext];
                        out.extend(
                            rings[1..]
                                .iter()
                                .map(|h| clip_ring_band(h, axis, min, max))
                                .filter(|h| !h.is_empty()),
                        );
                        Some(out)
                    })
                    .collect(),
            ),
        }
    }
}

fn to_unit(c: Coord<f64>) -> Coord<f64> {
    Coord {
        x: lon_to_unit_x(c.x),
        y: lat_to_unit_y(c.y),
    }
}

fn shape_of(geometry: &Geometry, as_lines: bool) -> Shape {
    let ring = |ls: &LineString<f64>| ls.0.iter().map(|&c| to_unit(c)).collect::<Ring>();
    let polygon_rings = |p: &geo_types::Polygon<f64>| {
        std::iter::once(p.exterior())
            .chain(p.interiors())
            .map(ring)
            .collect::<Vec<_>>()
    };
    match geometry {
        Geometry::Point(p) => Shape::Points(vec![to_unit(p.0)]),
        Geometry::MultiPoint(mp) => Shape::Points(mp.0.iter().map(|p| to_unit(p.0)).collect()),
        Geometry::Line(l) => Shape::Lines(vec![ring(l)]),
        Geometry::MultiLine(m) => Shape::Lines(m.0.iter().map(ring).collect()),
        Geometry::Polygon(p) if as_lines => Shape::Lines(polygon_rings(p)),
        Geometry::MultiPolygon(mp) if as_lines => {
            Shape::Lines(mp.0.iter().flat_map(polygon_rings).collect())
        }
        Geometry::Polygon(p) => Shape::Polygons(vec![polygon_rings(p)]),
        Geometry::MultiPolygon(mp) => Shape::Polygons(mp.0.iter().map(polygon_rings).collect()),
    }
}

/// Keys never written as plain attributes: classification keys are folded
/// into `class`.
///
/// Runs for every tag of every feature, so it compares against the layer
/// names directly rather than through `Layer::from_str`, which allocates an
/// error on every miss.
fn is_class_key(k: &str) -> bool {
    k == "waterway" || Layer::ALL.iter().any(|l| l.as_str() == k)
}

/// Keys emitted in [`AttributeMode::Curated`].
fn is_curated_attribute(k: &str) -> bool {
    !is_class_key(k) && osmic_osm::tags::CURATED_KEYS.contains(&k)
}

/// Renders features into tiles.
pub struct Renderer<'a> {
    config: &'a RenderConfig,
    tag_store: &'a TagStore,
}

impl<'a> Renderer<'a> {
    pub fn new(config: &'a RenderConfig, tag_store: &'a TagStore) -> Self {
        Self { config, tag_store }
    }

    /// Attributes for `feature`: `class` (the raw OSM value of its layer's
    /// key, falling back to the parsed kind) followed by the configured
    /// tags.
    pub fn attributes(&self, feature: &Feature) -> Vec<(String, String)> {
        let tags: Vec<(&str, &str)> = self.tag_store.resolve_tags(&feature.tags).collect();
        let layer = feature.kind.layer();
        // The water layer is fed by three keys; every other layer by the
        // key of the same name.
        let class_keys: &[&str] = match layer {
            Layer::Water => &["waterway", "water", "natural"],
            _ => &[],
        };
        let lookup = |key: &str| tags.iter().find(|(k, _)| *k == key).map(|(_, v)| *v);
        let class = class_keys
            .iter()
            .find_map(|k| lookup(k))
            .or_else(|| lookup(layer.as_str()))
            .unwrap_or(feature.kind.class_name());
        let mut out = vec![("class".to_string(), class.to_string())];
        for (k, v) in &tags {
            let keep = match self.config.attributes {
                AttributeMode::Curated => is_curated_attribute(k),
                AttributeMode::All => *k != "class",
            };
            if keep {
                out.push(((*k).to_string(), (*v).to_string()));
            }
        }
        out
    }

    /// Render `feature` into every tile it touches at every applicable
    /// zoom, calling `emit` for each piece.
    pub fn render(&self, feature: &Feature, emit: &mut dyn FnMut(RenderedFeature<'_>)) {
        let cfg = self.config;
        let zmin = cfg.min_zoom.max(feature.kind.min_zoom());
        if zmin > cfg.max_zoom || cfg.max_zoom > Zoom::MAX.get() {
            return;
        }
        let layer = feature.kind.layer();
        let mut unit = shape_of(&feature.geometry, layer == Layer::Boundary);
        if unit.is_empty() {
            return;
        }
        let ctx = TileContext {
            id: feature.id.vector_tile_id(),
            attributes: self.attributes(feature),
            layer,
            importance: feature.kind.importance(),
        };
        let extent = f64::from(cfg.extent);
        for z in (zmin..=cfg.max_zoom).rev() {
            let px_per_unit = 256.0 * f64::from(tiles_per_axis(z));
            let tol_px = if z == cfg.max_zoom {
                cfg.simplify_px_max_zoom
            } else {
                cfg.simplify_px
            };
            if tol_px > 0.0 {
                unit = unit.simplify(tol_px / px_per_unit);
                if unit.is_empty() {
                    return; // smaller at every lower zoom too
                }
            }
            let size = unit.size();
            let size_px = match unit {
                Shape::Points(_) => 0.0,
                Shape::Lines(_) => size * px_per_unit,
                Shape::Polygons(_) => size.sqrt() * px_per_unit,
            };
            if z < cfg.max_zoom && !matches!(unit, Shape::Points(_)) && size_px < cfg.min_size_px {
                return;
            }
            let size_class = ((size_px + 1.0).log2() * 2048.0).clamp(0.0, 65535.0) as u16;

            let scale = extent * f64::from(tiles_per_axis(z));
            let world = unit.map(|c| Coord {
                x: c.x * scale,
                y: c.y * scale,
            });
            let Some((lo, hi)) = world.bbox() else {
                return;
            };
            let buffer = cfg.buffer_px * extent / 256.0;
            let n = tiles_per_axis(z);
            let tile_index = |v: f64| -> u32 {
                let t = (v / extent).floor();
                if t.is_nan() || t < 0.0 {
                    0
                } else {
                    (t as u64).min(u64::from(n - 1)) as u32
                }
            };
            let range = TileSpan {
                x0: tile_index(lo.x - buffer),
                x1: tile_index(hi.x + buffer),
                y0: tile_index(lo.y - buffer),
                y1: tile_index(hi.y + buffer),
            };
            let zctx = ZoomContext {
                z,
                extent,
                buffer,
                size_class,
            };
            self.slice(&ctx, &zctx, world, range, emit);
        }
    }

    fn slice(
        &self,
        ctx: &TileContext,
        zctx: &ZoomContext,
        shape: Shape,
        span: TileSpan,
        emit: &mut dyn FnMut(RenderedFeature<'_>),
    ) {
        if shape.is_empty() {
            return;
        }
        let (e, b) = (zctx.extent, zctx.buffer);
        if span.x0 == span.x1 && span.y0 == span.y1 {
            let (x, y) = (span.x0, span.y0);
            let (ox, oy) = (f64::from(x) * e, f64::from(y) * e);
            let leaf = shape
                .clip(Axis::X, ox - b, ox + e + b)
                .clip(Axis::Y, oy - b, oy + e + b);
            if let Some((geom_type, parts)) = quantize(&leaf, ox, oy) {
                emit(RenderedFeature {
                    tile: TileCoord::new(x, y, Zoom::clamped(zctx.z)),
                    layer: ctx.layer,
                    importance: ctx.importance,
                    size_class: zctx.size_class,
                    id: ctx.id,
                    geom_type,
                    parts,
                    attributes: &ctx.attributes,
                });
            }
            return;
        }
        if span.x1 - span.x0 >= span.y1 - span.y0 {
            let mid = span.x0 + (span.x1 - span.x0) / 2;
            let split = f64::from(mid + 1) * e;
            let left = shape.clip(Axis::X, f64::from(span.x0) * e - b, split + b);
            let right = shape.clip(Axis::X, split - b, f64::from(span.x1 + 1) * e + b);
            drop(shape);
            self.slice(ctx, zctx, left, TileSpan { x1: mid, ..span }, emit);
            self.slice(
                ctx,
                zctx,
                right,
                TileSpan {
                    x0: mid + 1,
                    ..span
                },
                emit,
            );
        } else {
            let mid = span.y0 + (span.y1 - span.y0) / 2;
            let split = f64::from(mid + 1) * e;
            let top = shape.clip(Axis::Y, f64::from(span.y0) * e - b, split + b);
            let bottom = shape.clip(Axis::Y, split - b, f64::from(span.y1 + 1) * e + b);
            drop(shape);
            self.slice(ctx, zctx, top, TileSpan { y1: mid, ..span }, emit);
            self.slice(
                ctx,
                zctx,
                bottom,
                TileSpan {
                    y0: mid + 1,
                    ..span
                },
                emit,
            );
        }
    }
}

struct TileContext {
    id: Option<u64>,
    attributes: Vec<(String, String)>,
    layer: Layer,
    importance: u8,
}

struct ZoomContext {
    z: u8,
    extent: f64,
    buffer: f64,
    size_class: u16,
}

#[derive(Debug, Clone, Copy)]
struct TileSpan {
    x0: u32,
    x1: u32,
    y0: u32,
    y1: u32,
}

fn quantize_points(pts: &[Coord<f64>], ox: f64, oy: f64) -> Vec<[i32; 2]> {
    let mut out: Vec<[i32; 2]> = Vec::with_capacity(pts.len());
    for c in pts {
        // Clipped to the buffered tile, so these fit comfortably in i32.
        let p = [(c.x - ox).round() as i32, (c.y - oy).round() as i32];
        if out.last() != Some(&p) {
            out.push(p);
        }
    }
    out
}

/// Quantise a clipped shape to tile-local integers. Returns `None` if
/// nothing valid remains.
fn quantize(shape: &Shape, ox: f64, oy: f64) -> Option<(GeomType, Vec<Vec<[i32; 2]>>)> {
    let (geom_type, parts) = match shape {
        Shape::Points(p) => {
            let pts: Vec<[i32; 2]> = p
                .iter()
                .map(|c| [(c.x - ox).round() as i32, (c.y - oy).round() as i32])
                .collect();
            (
                GeomType::Point,
                if pts.is_empty() { vec![] } else { vec![pts] },
            )
        }
        Shape::Lines(l) => (
            GeomType::LineString,
            l.iter()
                .map(|p| quantize_points(p, ox, oy))
                .filter(|p| p.len() >= 2)
                .collect(),
        ),
        Shape::Polygons(polys) => {
            let mut parts = Vec::new();
            for rings in polys {
                let mut ring_iter = rings.iter().map(|r| {
                    let mut q = quantize_points(r, ox, oy);
                    while q.len() > 1 && q.first() == q.last() {
                        q.pop();
                    }
                    q
                });
                let Some(mut ext) = ring_iter.next() else {
                    continue;
                };
                let area = ring_area2(&ext);
                if ext.len() < 3 || area == 0 {
                    continue;
                }
                if area < 0 {
                    ext.reverse();
                }
                parts.push(ext);
                for mut hole in ring_iter {
                    let a = ring_area2(&hole);
                    if hole.len() < 3 || a == 0 {
                        continue;
                    }
                    if a > 0 {
                        hole.reverse();
                    }
                    parts.push(hole);
                }
            }
            (GeomType::Polygon, parts)
        }
    };
    (!parts.is_empty()).then_some((geom_type, parts))
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo_types::{LineString, Point, Polygon};
    use osmic_core::OsmId;
    use osmic_osm::feature::{AmenityKind, BoundaryKind, FeatureKind, HighwayKind, LanduseKind};
    use osmic_osm::{TagRetention, Tags};

    fn feature(
        kind: FeatureKind,
        geometry: Geometry,
        tags: &[(&str, &str)],
        store: &TagStore,
    ) -> Feature {
        Feature {
            id: OsmId::way(7),
            kind,
            geometry,
            tags: store.intern_tags(tags.iter().copied(), &TagRetention::All),
        }
    }

    /// An owned [`RenderedFeature`].
    struct Piece {
        tile: TileCoord,
        feature: TileFeature,
    }

    fn render_all(cfg: &RenderConfig, store: &TagStore, f: &Feature) -> Vec<Piece> {
        let mut out = Vec::new();
        Renderer::new(cfg, store).render(f, &mut |r| {
            out.push(Piece {
                tile: r.tile,
                feature: r.into_tile_feature(),
            });
        });
        out
    }

    #[test]
    fn point_lands_in_the_right_tile_with_exact_coordinates() {
        let store = TagStore::new();
        let cfg = RenderConfig {
            min_zoom: 14,
            max_zoom: 14,
            ..Default::default()
        };
        let f = feature(
            FeatureKind::Amenity(AmenityKind::Cafe),
            Geometry::Point(Point::new(-122.4194, 37.7749)),
            &[("amenity", "cafe"), ("name", "Blue Bottle"), ("fixme", "x")],
            &store,
        );
        let out = render_all(&cfg, &store, &f);
        assert_eq!(out.len(), 1);
        let r = &out[0];
        let (tx, ty) = osmic_core::mercator::lonlat_to_tile(-122.4194, 37.7749, 14);
        assert_eq!((r.tile.x, r.tile.y), (tx, ty));
        let [[x, y]] = r.feature.parts[0][..] else {
            panic!()
        };
        assert!((0..4096).contains(&x) && (0..4096).contains(&y));
        assert_eq!(r.feature.id, OsmId::way(7).vector_tile_id());
        assert_eq!(
            r.feature.attributes,
            [
                ("class".to_string(), "cafe".to_string()),
                ("name".to_string(), "Blue Bottle".to_string())
            ],
            "curated attributes only"
        );
    }

    #[test]
    fn line_crossing_tiles_is_split_with_buffers() {
        let store = TagStore::new();
        let cfg = RenderConfig {
            min_zoom: 4,
            max_zoom: 4,
            simplify_px_max_zoom: 0.0,
            ..Default::default()
        };
        // A motorway crossing four z4 tiles east-west near the equator.
        let f = feature(
            FeatureKind::Highway(HighwayKind::Motorway),
            Geometry::Line(LineString::from(vec![(-30.0, 1.0), (60.0, 1.0)])),
            &[("highway", "motorway")],
            &store,
        );
        let out = render_all(&cfg, &store, &f);
        let mut xs: Vec<u32> = out.iter().map(|r| r.tile.x).collect();
        xs.sort_unstable();
        // -30° … 60° spans tiles 6..=10 at z4.
        assert_eq!(xs, [6, 7, 8, 9, 10]);
        let buffer = (4.0 * 4096.0 / 256.0) as i32;
        for r in &out {
            for &[x, _] in r.feature.parts.iter().flatten() {
                assert!(
                    (-buffer..=4096 + buffer).contains(&x),
                    "x {x} outside buffered tile"
                );
            }
        }
        // Interior tiles see the line cross the whole buffered width.
        let mid = out.iter().find(|r| r.tile.x == 8).expect("tile 8");
        let part = &mid.feature.parts[0];
        assert_eq!(part.first().map(|p| p[0]), Some(-buffer));
        assert_eq!(part.last().map(|p| p[0]), Some(4096 + buffer));
    }

    #[test]
    fn polygon_winding_follows_mvt() {
        let store = TagStore::new();
        let cfg = RenderConfig {
            min_zoom: 10,
            max_zoom: 10,
            ..Default::default()
        };
        // CCW in lon/lat (osmic's convention) with a CW hole.
        let poly = Polygon::new(
            LineString::from(vec![
                (0.0, 0.0),
                (0.2, 0.0),
                (0.2, 0.2),
                (0.0, 0.2),
                (0.0, 0.0),
            ]),
            vec![LineString::from(vec![
                (0.05, 0.05),
                (0.05, 0.1),
                (0.1, 0.1),
                (0.1, 0.05),
                (0.05, 0.05),
            ])],
        );
        let f = feature(
            FeatureKind::Landuse(LanduseKind::Grass),
            Geometry::Polygon(poly),
            &[("landuse", "grass")],
            &store,
        );
        for r in render_all(&cfg, &store, &f) {
            let ext = &r.feature.parts[0];
            assert!(
                ring_area2(ext) > 0,
                "exterior must be positive in tile space"
            );
            for hole in &r.feature.parts[1..] {
                assert!(ring_area2(hole) < 0, "holes must be negative");
            }
            assert!(ext.first() != ext.last(), "no closing vertex in MVT rings");
        }
    }

    #[test]
    fn boundaries_render_as_lines() {
        let store = TagStore::new();
        let cfg = RenderConfig {
            min_zoom: 5,
            max_zoom: 5,
            ..Default::default()
        };
        let f = feature(
            FeatureKind::Boundary(BoundaryKind::Administrative),
            Geometry::Polygon(Polygon::new(
                LineString::from(vec![
                    (0.0, 0.0),
                    (5.0, 0.0),
                    (5.0, 5.0),
                    (0.0, 5.0),
                    (0.0, 0.0),
                ]),
                vec![],
            )),
            &[("boundary", "administrative"), ("admin_level", "4")],
            &store,
        );
        let out = render_all(&cfg, &store, &f);
        assert!(!out.is_empty());
        assert!(
            out.iter()
                .all(|r| r.feature.geom_type == GeomType::LineString)
        );
        assert!(
            out[0]
                .feature
                .attributes
                .iter()
                .any(|(k, v)| k == "admin_level" && v == "4")
        );
    }

    #[test]
    fn tiny_features_are_dropped_below_max_zoom_but_kept_at_max() {
        let store = TagStore::new();
        let cfg = RenderConfig {
            min_zoom: 0,
            max_zoom: 14,
            ..Default::default()
        };
        // ~10 m square: a few pixels at z14, invisible at z6.
        let s = 0.0001;
        let f = feature(
            FeatureKind::Landuse(LanduseKind::Grass),
            Geometry::Polygon(Polygon::new(
                LineString::from(vec![(0.0, 0.0), (s, 0.0), (s, s), (0.0, s), (0.0, 0.0)]),
                vec![],
            )),
            &[("landuse", "grass")],
            &store,
        );
        let zooms: Vec<u8> = render_all(&cfg, &store, &f)
            .iter()
            .map(|r| r.tile.z.get())
            .collect();
        assert!(zooms.contains(&14));
        assert!(
            !zooms.contains(&7),
            "landuse min zoom 7, but too small there: {zooms:?}"
        );
    }

    #[test]
    fn min_zoom_of_kind_is_respected() {
        let store = TagStore::new();
        let cfg = RenderConfig::default();
        let f = feature(
            FeatureKind::Highway(HighwayKind::Residential),
            Geometry::Line(LineString::from(vec![(0.0, 0.0), (0.05, 0.0)])),
            &[("highway", "residential")],
            &store,
        );
        let zooms: Vec<u8> = render_all(&cfg, &store, &f)
            .iter()
            .map(|r| r.tile.z.get())
            .collect();
        assert!(zooms.iter().all(|&z| z >= 12), "{zooms:?}");
        assert!(zooms.contains(&12) && zooms.contains(&14));
    }

    #[test]
    fn feature_near_tile_edge_reaches_neighbour_buffer() {
        let store = TagStore::new();
        let cfg = RenderConfig {
            min_zoom: 14,
            max_zoom: 14,
            ..Default::default()
        };
        // A point 1 px (1/256 tile) west of a z14 tile edge.
        let tile_w = 360.0 / 16384.0;
        let lon = -180.0 + 100.0 * tile_w - tile_w / 256.0;
        let f = feature(
            FeatureKind::Amenity(AmenityKind::Cafe),
            Geometry::Point(Point::new(lon, 0.01)),
            &[("amenity", "cafe")],
            &store,
        );
        let xs: Vec<u32> = render_all(&cfg, &store, &f)
            .iter()
            .map(|r| r.tile.x)
            .collect();
        assert_eq!(xs.len(), 2, "own tile and the neighbour's buffer: {xs:?}");
        assert!(xs.contains(&99) && xs.contains(&100));
    }

    #[test]
    fn class_keys_are_exactly_the_layer_names_and_waterway() {
        for layer in Layer::ALL {
            assert!(is_class_key(layer.as_str()), "{layer}");
        }
        assert!(is_class_key("waterway"));
        for key in [
            "name",
            "ref",
            "class",
            "Highway",
            "highway ",
            "",
            "water_way",
        ] {
            assert_eq!(is_class_key(key), key.parse::<Layer>().is_ok(), "{key:?}");
            assert!(!is_class_key(key), "{key:?}");
        }
    }

    #[test]
    fn empty_tags_still_get_a_class() {
        let store = TagStore::new();
        let f = Feature {
            id: OsmId::node(1),
            kind: FeatureKind::Amenity(AmenityKind::Bank),
            geometry: Geometry::Point(Point::new(0.0, 0.0)),
            tags: Tags::new(),
        };
        let attrs = Renderer::new(&RenderConfig::default(), &store).attributes(&f);
        assert_eq!(attrs, [("class".to_string(), "bank".to_string())]);
    }
}
