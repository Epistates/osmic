//! Style evaluation: [`Style`] + decoded features → [`SceneGraph`].
//!
//! This is the one place where a style meets data. Both the software
//! renderer and the viewer's tessellator consume its output, so a style
//! change shows up identically everywhere.

use std::collections::HashMap;
use std::sync::Arc;

use geo_types::{Coord, LineString, Polygon};
use osmic_core::{Color, Geometry, TileCoord};
use osmic_style::{
    Alignment, EvalContext, Layer, LayerKind, PropertySource, Style, SymbolPlacement, SymbolStyle,
    ValueRef,
};
use osmic_text::{FontStack, LabelAnchor, LabelCandidate, LabelStyle};
use osmic_tiles::mvt_decode::{AttrRef, DecodedFeature};

use crate::camera::{PixelMapping, TILE_SIZE};
use crate::scene::{RenderFeature, RenderLayer, SceneGraph};
use crate::tessellate::MIN_DASH_PERIOD;

/// Parameters of one scene build.
///
/// Build with [`SceneOptions::new`] or [`SceneOptions::for_tile`] and the
/// `with_*` methods.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SceneOptions {
    /// Style zoom (MapLibre convention) the style is evaluated at.
    pub zoom: f64,
    /// Projection from the Mercator unit square into scene pixels.
    pub mapping: PixelMapping,
    /// Rectangle `[x0, y0, x1, y1]` in scene pixels. Primitives entirely
    /// outside it (allowing for stroke width) are dropped. `None` keeps
    /// everything.
    pub cull: Option<[f32; 4]>,
    /// Rectangle every produced layer is clipped to by the renderer; set to
    /// a tile's bounds when composing a view from several tiles.
    pub clip: Option<[f32; 4]>,
}

impl SceneOptions {
    /// Evaluate at `zoom`, projecting with `mapping`; no culling or
    /// clipping.
    pub fn new(zoom: f64, mapping: PixelMapping) -> Self {
        Self {
            zoom,
            mapping,
            cull: None,
            clip: None,
        }
    }

    /// One tile on its own: the style at the tile's zoom, tile-local
    /// pixels ([`PixelMapping::for_tile`]) and the scene clipped to the
    /// tile's `TILE_SIZE` square.
    pub fn for_tile(tile: TileCoord) -> Self {
        let size = TILE_SIZE as f32;
        Self::new(f64::from(tile.z.0), PixelMapping::for_tile(tile))
            .with_clip([0.0, 0.0, size, size])
    }

    /// Drop primitives entirely outside `rect` (`[x0, y0, x1, y1]`).
    pub fn with_cull(mut self, rect: [f32; 4]) -> Self {
        self.cull = Some(rect);
        self
    }

    /// Clip every produced layer to `rect` (`[x0, y0, x1, y1]`).
    pub fn with_clip(mut self, rect: [f32; 4]) -> Self {
        self.clip = Some(rect);
        self
    }

    /// The area a `background` layer covers: the clip rectangle, else the
    /// cull rectangle, else the whole projected world.
    fn background_rect(&self) -> [f32; 4] {
        self.clip.or(self.cull).unwrap_or_else(|| {
            let PixelMapping { origin, scale } = self.mapping;
            [
                (-origin[0] * scale) as f32,
                (-origin[1] * scale) as f32,
                ((1.0 - origin[0]) * scale) as f32,
                ((1.0 - origin[1]) * scale) as f32,
            ]
        })
    }
}

/// Feature attributes as seen by style expressions, with their tile types:
/// numbers are numbers and booleans booleans, so MapLibre filters such as
/// `["==", ["get", "admin_level"], 2]` match. Lookups borrow; nothing is
/// allocated.
struct FeatureProps<'a>(&'a DecodedFeature);

impl PropertySource for FeatureProps<'_> {
    fn property(&self, key: &str) -> Option<ValueRef<'_>> {
        self.0.attribute(key).map(|v| match v {
            AttrRef::String(s) => ValueRef::String(s),
            AttrRef::Number(n) => ValueRef::Number(n),
            AttrRef::Bool(b) => ValueRef::Bool(b),
            // A value type newer than this renderer: unreadable, so null.
            _ => ValueRef::Null,
        })
    }
}

/// Builds scenes from one style.
pub struct SceneBuilder<'a> {
    style: &'a Style,
}

/// Convenience for `SceneBuilder::new(style).build(features, options)`.
pub fn build_scene(
    style: &Style,
    features: &[DecodedFeature],
    options: &SceneOptions,
) -> SceneGraph {
    SceneBuilder::new(style).build(features, options)
}

impl<'a> SceneBuilder<'a> {
    pub fn new(style: &'a Style) -> Self {
        Self { style }
    }

    /// Evaluate the style over `features`.
    ///
    /// Layers are visited in style order; within a layer, features keep
    /// their input order. Label candidates carry a priority derived from the
    /// layer order (later layers win collisions) and `symbol-sort-key`.
    ///
    /// `background` layers paint in style order, as in MapLibre. The ones
    /// below every other active layer are composited into the scene's
    /// clear color ([`SceneGraph::background`]; transparent if there are
    /// none); any later one becomes a layer filling
    /// [`SceneOptions::clip`], else [`SceneOptions::cull`], else the
    /// projected world.
    pub fn build(&self, features: &[DecodedFeature], options: &SceneOptions) -> SceneGraph {
        let zoom = options.zoom;
        let mut by_layer: HashMap<&str, Vec<&DecodedFeature>> = HashMap::new();
        for f in features {
            by_layer.entry(f.layer.as_str()).or_default().push(f);
        }

        let mut scene = SceneGraph::new(Color::TRANSPARENT);
        // Whether every active layer so far was a background. This depends
        // on the style only, never on the data, so all tiles of a view
        // agree on which backgrounds are the clear color.
        let mut below_everything = true;
        let layer_count = self.style.layers.len();
        for (index, layer) in self.style.layers.iter().enumerate() {
            if !layer.is_active_at(zoom) {
                continue;
            }
            let mut out = RenderLayer::new(index as i32);
            out.clip = options.clip;
            if let LayerKind::Background(bg) = &layer.kind {
                let color = bg.resolve(&EvalContext::at_zoom(zoom));
                if below_everything {
                    scene.background = over(color, scene.background);
                } else if color.a > 0.0 {
                    let [x0, y0, x1, y1] = options.background_rect();
                    out.push(RenderFeature::Fill {
                        coords: vec![vec![[x0, y0], [x1, y0], [x1, y1], [x0, y1]]],
                        color,
                    });
                    scene.add_layer(out);
                }
                continue;
            }
            below_everything = false;
            let Some(source_layer) = layer.source_layer.as_deref() else {
                continue;
            };
            let Some(feats) = by_layer.get(source_layer) else {
                continue;
            };
            let rank = (layer_count - 1 - index) as u32;
            build_layer(layer, feats, options, rank, &mut out);
            if !out.features.is_empty() {
                scene.add_layer(out);
            }
        }
        scene
    }
}

/// `top` composited over `bottom` (source-over), as straight-alpha colors.
fn over(top: Color, bottom: Color) -> Color {
    let (t, b) = (top.premultiplied(), bottom.premultiplied());
    let inv = 1.0 - t[3];
    let p: [f32; 4] = std::array::from_fn(|i| t[i] + b[i] * inv);
    if p[3] <= 0.0 {
        Color::TRANSPARENT
    } else {
        Color::rgba(p[0] / p[3], p[1] / p[3], p[2] / p[3], p[3])
    }
}

fn build_layer(
    layer: &Layer,
    feats: &[&DecodedFeature],
    options: &SceneOptions,
    rank: u32,
    out: &mut RenderLayer,
) {
    let zoom = options.zoom;
    let m = &options.mapping;
    let data_driven = layer.kind.is_data_driven();
    let sizes_zoom = layer.kind.size_depends_on_zoom();
    let next_ctx = EvalContext::at_zoom(zoom + 1.0);
    let constant_ctx = EvalContext::at_zoom(zoom);

    match &layer.kind {
        LayerKind::Background(_) => {}
        LayerKind::Fill(l) => {
            let constant = (!data_driven).then(|| l.resolve(&constant_ctx));
            for f in feats {
                let props = FeatureProps(f);
                let ctx = EvalContext::new(zoom, &props);
                if !layer.accepts(&ctx) {
                    continue;
                }
                let style = constant.unwrap_or_else(|| l.resolve(&ctx));
                if style.color.a <= 0.0 {
                    continue;
                }
                for poly in polygons(&f.geometry) {
                    if let Some(rings) = project_polygon(poly, m)
                        && visible(&options.cull, rings.iter().flatten(), 0.0)
                    {
                        out.push(RenderFeature::Fill {
                            coords: rings,
                            color: style.color,
                        });
                    }
                }
            }
        }
        LayerKind::Line(l) => {
            let constant = (!data_driven).then(|| l.resolve(&constant_ctx));
            let constant_next = (!data_driven && sizes_zoom).then(|| l.resolve(&next_ctx).width);
            for f in feats {
                let props = FeatureProps(f);
                let ctx = EvalContext::new(zoom, &props);
                if !layer.accepts(&ctx) {
                    continue;
                }
                let owned;
                let style = match &constant {
                    Some(s) => s,
                    None => {
                        owned = l.resolve(&ctx);
                        &owned
                    }
                };
                if style.color.a <= 0.0 || style.width <= 0.0 {
                    continue;
                }
                let width_next = match constant_next {
                    Some(w) => w,
                    None if sizes_zoom => l.resolve(&EvalContext::new(zoom + 1.0, &props)).width,
                    None => style.width,
                };
                let mut dash = dash_pattern(&style.dasharray, style.width);
                let reach = style.width.max(width_next);
                let mut parts = lines(&f.geometry, m)
                    .into_iter()
                    .filter(|coords| visible(&options.cull, coords.iter(), reach))
                    .peekable();
                while let Some(coords) = parts.next() {
                    // Only a multi-part line needs copies of the pattern;
                    // the last part takes it.
                    let dash = if parts.peek().is_some() {
                        dash.clone()
                    } else {
                        std::mem::take(&mut dash)
                    };
                    out.push(RenderFeature::Stroke {
                        coords,
                        color: style.color,
                        width: style.width,
                        width_next_zoom: width_next,
                        cap: style.cap,
                        join: style.join,
                        dash,
                    });
                }
            }
        }
        LayerKind::Circle(l) => {
            let constant = (!data_driven).then(|| l.resolve(&constant_ctx));
            let constant_next = (!data_driven && sizes_zoom).then(|| l.resolve(&next_ctx).radius);
            for f in feats {
                let props = FeatureProps(f);
                let ctx = EvalContext::new(zoom, &props);
                if !layer.accepts(&ctx) {
                    continue;
                }
                let style = constant.unwrap_or_else(|| l.resolve(&ctx));
                // The stroke lies outside the radius, so a zero-radius
                // circle with a stroke is still a (stroke-colored) dot.
                let fill_visible = style.radius > 0.0 && style.color.a > 0.0;
                let stroke_visible = style.stroke_width > 0.0 && style.stroke_color.a > 0.0;
                if !(fill_visible || stroke_visible) {
                    continue;
                }
                let radius_next = match constant_next {
                    Some(r) => r,
                    None if sizes_zoom => l.resolve(&EvalContext::new(zoom + 1.0, &props)).radius,
                    None => style.radius,
                };
                for center in points(&f.geometry, m) {
                    let reach = style.radius.max(radius_next) + style.stroke_width;
                    if visible(&options.cull, std::iter::once(&center), reach) {
                        out.push(RenderFeature::Circle {
                            center,
                            radius: style.radius,
                            radius_next_zoom: radius_next,
                            color: style.color,
                            stroke_color: style.stroke_color,
                            stroke_width: style.stroke_width,
                        });
                    }
                }
            }
        }
        LayerKind::Symbol(l) => {
            let constant = (!data_driven).then(|| l.resolve(&constant_ctx));
            for f in feats {
                let props = FeatureProps(f);
                let ctx = EvalContext::new(zoom, &props);
                if !layer.accepts(&ctx) {
                    continue;
                }
                let owned;
                let style = match &constant {
                    Some(s) => s,
                    None => {
                        owned = l.resolve(&ctx);
                        &owned
                    }
                };
                if style.text.trim().is_empty() || style.size < 1.0 {
                    continue;
                }
                let anchors =
                    label_anchors(&f.geometry, style.placement, style.rotation_alignment, m);
                for anchor in anchors {
                    let in_view = match &anchor {
                        LabelAnchor::Point(p) => visible(&options.cull, std::iter::once(p), 0.0),
                        LabelAnchor::Line(line) | LabelAnchor::LineCenter(line) => {
                            visible(&options.cull, line.iter(), 0.0)
                        }
                        _ => true,
                    };
                    if in_view {
                        out.push(RenderFeature::Label(label_candidate(style, anchor, rank)));
                    }
                }
            }
        }
    }
}

fn label_style(style: &SymbolStyle) -> LabelStyle {
    let mut s = LabelStyle::new(style.size, style.color);
    s.font = FontStack::from(Arc::clone(&style.font));
    if style.halo_color.a > 0.0 {
        s = s.with_halo(style.halo_color, style.halo_width);
    }
    s.anchor = style.anchor.fraction();
    s.offset = [style.offset[0] * style.size, style.offset[1] * style.size];
    s.padding = style.padding;
    s.allow_overlap = style.allow_overlap;
    s.max_angle_degrees = style.max_angle_degrees;
    s
}

fn label_candidate(style: &SymbolStyle, anchor: LabelAnchor, rank: u32) -> LabelCandidate {
    LabelCandidate {
        text: style.text.clone(),
        anchor,
        style: label_style(style),
        layer_rank: rank,
        sort_key: style.sort_key,
    }
}

// --- geometry ---------------------------------------------------------

fn polygons(g: &Geometry) -> Vec<&Polygon<f64>> {
    match g {
        Geometry::Polygon(p) => vec![p],
        Geometry::MultiPolygon(mp) => mp.0.iter().collect(),
        _ => Vec::new(),
    }
}

fn project_ring(
    ring: &LineString<f64>,
    m: &PixelMapping,
    keep_closing_point: bool,
) -> Vec<[f32; 2]> {
    let mut out: Vec<[f32; 2]> = Vec::with_capacity(ring.0.len());
    for &Coord { x, y } in &ring.0 {
        let p = m.project(x, y);
        if p[0].is_finite() && p[1].is_finite() && out.last() != Some(&p) {
            out.push(p);
        }
    }
    if !keep_closing_point && out.len() > 1 && out.first() == out.last() {
        out.pop();
    }
    out
}

/// Exterior plus holes, dropping degenerate rings; `None` if the exterior
/// is degenerate.
fn project_polygon(p: &Polygon<f64>, m: &PixelMapping) -> Option<Vec<Vec<[f32; 2]>>> {
    let exterior = project_ring(p.exterior(), m, false);
    if exterior.len() < 3 {
        return None;
    }
    let mut rings = vec![exterior];
    rings.extend(
        p.interiors()
            .iter()
            .map(|r| project_ring(r, m, false))
            .filter(|r| r.len() >= 3),
    );
    Some(rings)
}

/// Polylines to stroke: line parts, and the rings of polygons (closed).
fn lines(g: &Geometry, m: &PixelMapping) -> Vec<Vec<[f32; 2]>> {
    let line = |ls: &LineString<f64>| {
        let pts = project_ring(ls, m, true);
        (pts.len() >= 2).then_some(pts)
    };
    let ring = |ls: &LineString<f64>| {
        let mut pts = project_ring(ls, m, false);
        if pts.len() < 3 {
            return None;
        }
        pts.push(pts[0]);
        Some(pts)
    };
    let polygon_rings = |p: &Polygon<f64>| {
        std::iter::once(p.exterior())
            .chain(p.interiors())
            .filter_map(ring)
            .collect::<Vec<_>>()
    };
    match g {
        Geometry::Line(ls) => line(ls).into_iter().collect(),
        Geometry::MultiLine(ml) => ml.0.iter().filter_map(line).collect(),
        Geometry::Polygon(p) => polygon_rings(p),
        Geometry::MultiPolygon(mp) => mp.0.iter().flat_map(polygon_rings).collect(),
        Geometry::Point(_) | Geometry::MultiPoint(_) => Vec::new(),
    }
}

fn points(g: &Geometry, m: &PixelMapping) -> Vec<[f32; 2]> {
    match g {
        Geometry::Point(p) => vec![m.project(p.x(), p.y())],
        Geometry::MultiPoint(mp) => mp.0.iter().map(|p| m.project(p.x(), p.y())).collect(),
        _ => Vec::new(),
    }
}

/// Where labels attach for a geometry under a placement mode and
/// `text-rotation-alignment`.
///
/// Scenes have no bearing, so for `point` placement `map` and `viewport`
/// alignment coincide (text is horizontal). Along lines, `map` and `auto`
/// rotate glyphs with the line, while `viewport` keeps the text horizontal
/// at the middle of each line (`line-center`: of each line part).
fn label_anchors(
    g: &Geometry,
    placement: SymbolPlacement,
    alignment: Alignment,
    m: &PixelMapping,
) -> Vec<LabelAnchor> {
    let follows_line = alignment != Alignment::Viewport;
    match placement {
        SymbolPlacement::Line | SymbolPlacement::LineCenter if !follows_line => lines(g, m)
            .iter()
            .filter_map(|l| line_midpoint(l))
            .map(LabelAnchor::Point)
            .collect(),
        SymbolPlacement::Line => lines(g, m).into_iter().map(LabelAnchor::Line).collect(),
        SymbolPlacement::LineCenter => lines(g, m)
            .into_iter()
            .map(LabelAnchor::LineCenter)
            .collect(),
        // `point`, and any placement newer than this renderer.
        _ => match g {
            Geometry::Point(_) | Geometry::MultiPoint(_) => {
                points(g, m).into_iter().map(LabelAnchor::Point).collect()
            }
            Geometry::Line(_) | Geometry::MultiLine(_) => lines(g, m)
                .iter()
                .filter_map(|l| line_midpoint(l))
                .map(LabelAnchor::Point)
                .collect(),
            Geometry::Polygon(p) => interior_point(p, m)
                .into_iter()
                .map(LabelAnchor::Point)
                .collect(),
            Geometry::MultiPolygon(mp) => {
                // Label the largest part only; islands would otherwise
                // repeat the name.
                mp.0.iter()
                    .max_by(|a, b| ring_area(a.exterior()).total_cmp(&ring_area(b.exterior())))
                    .and_then(|p| interior_point(p, m))
                    .into_iter()
                    .map(LabelAnchor::Point)
                    .collect()
            }
        },
    }
}

fn ring_area(ring: &LineString<f64>) -> f64 {
    let c = &ring.0;
    (0..c.len().saturating_sub(1))
        .map(|i| c[i].x * c[i + 1].y - c[i + 1].x * c[i].y)
        .sum::<f64>()
        .abs()
}

fn line_midpoint(line: &[[f32; 2]]) -> Option<[f32; 2]> {
    let total: f32 = line
        .windows(2)
        .map(|w| (w[1][0] - w[0][0]).hypot(w[1][1] - w[0][1]))
        .sum();
    let mut remaining = total / 2.0;
    for w in line.windows(2) {
        let seg = (w[1][0] - w[0][0]).hypot(w[1][1] - w[0][1]);
        if remaining <= seg && seg > 0.0 {
            let t = remaining / seg;
            return Some([
                w[0][0] + (w[1][0] - w[0][0]) * t,
                w[0][1] + (w[1][1] - w[0][1]) * t,
            ]);
        }
        remaining -= seg;
    }
    line.first().copied()
}

/// A point guaranteed to lie inside the polygon: the middle of the widest
/// horizontal span found on a handful of scanlines.
fn interior_point(p: &Polygon<f64>, m: &PixelMapping) -> Option<[f32; 2]> {
    let rings = project_polygon(p, m)?;
    let (mut min_y, mut max_y) = (f32::MAX, f32::MIN);
    for pt in &rings[0] {
        min_y = min_y.min(pt[1]);
        max_y = max_y.max(pt[1]);
    }
    let mut best: Option<(f32, [f32; 2])> = None;
    for frac in [0.5, 0.4, 0.6, 0.3, 0.7, 0.2, 0.8] {
        let y = min_y + (max_y - min_y) * frac;
        let mut xs: Vec<f32> = Vec::new();
        for ring in &rings {
            for i in 0..ring.len() {
                let (a, b) = (ring[i], ring[(i + 1) % ring.len()]);
                if (a[1] <= y) != (b[1] <= y) {
                    xs.push(a[0] + (y - a[1]) / (b[1] - a[1]) * (b[0] - a[0]));
                }
            }
        }
        xs.sort_by(f32::total_cmp);
        for pair in xs.as_chunks::<2>().0.iter() {
            let width = pair[1] - pair[0];
            if best.is_none_or(|(w, _)| width > w) {
                best = Some((width, [(pair[0] + pair[1]) / 2.0, y]));
            }
        }
    }
    best.map(|(_, pt)| pt)
}

/// Dash pattern in pixels from a `line-dasharray` (multiples of the line
/// width). Odd-length patterns repeat so on/off alternate consistently.
/// Invalid patterns, and patterns repeating more often than
/// [`MIN_DASH_PERIOD`] pixels, are solid.
fn dash_pattern(dasharray: &[f32], width: f32) -> Vec<f32> {
    if dasharray.is_empty() || dasharray.iter().any(|d| !d.is_finite() || *d < 0.0) {
        return Vec::new();
    }
    let period: f32 = dasharray.iter().sum::<f32>() * width;
    let period = if dasharray.len() % 2 == 1 {
        2.0 * period
    } else {
        period
    };
    if !(period.is_finite() && period >= MIN_DASH_PERIOD) {
        return Vec::new();
    }
    let mut pattern: Vec<f32> = Vec::with_capacity(dasharray.len() * 2);
    pattern.extend(dasharray.iter().map(|d| d * width));
    if dasharray.len() % 2 == 1 {
        pattern.extend(dasharray.iter().map(|d| d * width));
    }
    pattern
}

/// Whether the points' bounds, grown by `margin`, touch the cull rect.
fn visible<'p>(
    cull: &Option<[f32; 4]>,
    points: impl Iterator<Item = &'p [f32; 2]>,
    margin: f32,
) -> bool {
    let Some([cx0, cy0, cx1, cy1]) = *cull else {
        return true;
    };
    let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    let mut any = false;
    for p in points {
        any = true;
        x0 = x0.min(p[0]);
        y0 = y0.min(p[1]);
        x1 = x1.max(p[0]);
        y1 = y1.max(p[1]);
    }
    any && x1 + margin >= cx0 && x0 - margin <= cx1 && y1 + margin >= cy0 && y0 - margin <= cy1
}

#[cfg(test)]
mod tests {
    use geo_types::{MultiPolygon, Point};
    use osmic_core::{TileCoord, Zoom};
    use osmic_style::{default_style, default_style_json};

    use super::*;

    fn feature(layer: &str, class: &str, name: Option<&str>, geometry: Geometry) -> DecodedFeature {
        DecodedFeature {
            layer: layer.into(),
            id: None,
            class: Some(class.into()),
            name: name.map(str::to_string),
            tags: vec![],
            geometry,
        }
    }

    fn tile_options(zoom: f64) -> (TileCoord, SceneOptions) {
        let tile = TileCoord::new(1309, 3166, Zoom(13));
        (
            tile,
            SceneOptions {
                zoom,
                mapping: PixelMapping::for_tile(tile),
                cull: None,
                clip: None,
            },
        )
    }

    fn square(tile: TileCoord, hole: bool) -> Geometry {
        let bb = tile.bbox();
        let (w, h) = (bb.width(), bb.height());
        let at = |fx: f64, fy: f64| (bb.min_lon + w * fx, bb.min_lat + h * fy);
        let ext = LineString::from(vec![
            at(0.1, 0.1),
            at(0.9, 0.1),
            at(0.9, 0.9),
            at(0.1, 0.9),
            at(0.1, 0.1),
        ]);
        let holes = if hole {
            vec![LineString::from(vec![
                at(0.4, 0.4),
                at(0.6, 0.4),
                at(0.6, 0.6),
                at(0.4, 0.6),
                at(0.4, 0.4),
            ])]
        } else {
            vec![]
        };
        Geometry::Polygon(Polygon::new(ext, holes))
    }

    fn road(tile: TileCoord) -> Geometry {
        let bb = tile.bbox();
        let y = bb.min_lat + bb.height() * 0.5;
        Geometry::Line(LineString::from(vec![
            (bb.min_lon + bb.width() * 0.05, y),
            (bb.min_lon + bb.width() * 0.95, y),
        ]))
    }

    fn fills(scene: &SceneGraph) -> Vec<(&Vec<Vec<[f32; 2]>>, Color)> {
        scene
            .layers
            .iter()
            .flat_map(|l| &l.features)
            .filter_map(|f| match f {
                RenderFeature::Fill { coords, color } => Some((coords, *color)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn background_comes_from_the_style() {
        let style = default_style();
        let (_, opts) = tile_options(13.0);
        let scene = build_scene(&style, &[], &opts);
        assert_eq!(scene.background.to_css(), "#f8f4f0");
        assert!(scene.layers.is_empty());
    }

    #[test]
    fn fill_color_follows_class_and_keeps_holes() {
        let style = default_style();
        let (tile, opts) = tile_options(13.0);
        let features = [
            feature("landuse", "forest", None, square(tile, true)),
            feature("landuse", "unknown-class", None, square(tile, false)),
        ];
        let scene = build_scene(&style, &features, &opts);
        let f = fills(&scene);
        assert_eq!(f.len(), 2);
        assert_eq!(f[0].1.to_rgba8()[..3], [0xad, 0xd1, 0x9e]);
        assert_eq!(f[1].1.to_rgba8()[..3], [0xd5, 0xcf, 0xc8]);
        assert_eq!(f[0].0.len(), 2, "exterior + hole");
        assert_eq!(f[1].0.len(), 1);
        // Rings are tile-local pixels and open (no duplicated closing point).
        let ext = &f[0].0[0];
        assert_eq!(ext.len(), 4);
        assert!(
            ext.iter()
                .all(|p| (0.0..=512.0).contains(&p[0]) && (0.0..=512.0).contains(&p[1]))
        );
    }

    #[test]
    fn layers_outside_their_zoom_range_are_skipped() {
        let style = default_style();
        let (tile, _) = tile_options(13.0);
        let features = [feature("building", "yes", None, square(tile, false))];
        let at = |zoom| {
            let (_, opts) = tile_options(zoom);
            build_scene(&style, &features, &opts)
        };
        assert!(at(12.0).layers.is_empty(), "buildings start at z13");
        assert!(!at(13.0).layers.is_empty());
    }

    #[test]
    fn filters_select_features() {
        let style = default_style();
        let (tile, opts) = tile_options(12.0);
        let features = [
            feature("water", "river", None, road(tile)),
            feature("water", "lake", None, road(tile)),
        ];
        let scene = build_scene(&style, &features, &opts);
        let strokes = scene
            .layers
            .iter()
            .flat_map(|l| &l.features)
            .filter(|f| matches!(f, RenderFeature::Stroke { .. }))
            .count();
        assert_eq!(strokes, 1, "water-line filters on class");
    }

    #[test]
    fn line_width_is_evaluated_per_class_and_zoom() {
        let style = default_style();
        let (tile, _) = tile_options(12.0);
        let features = [
            feature("highway", "motorway", None, road(tile)),
            feature("highway", "residential", None, road(tile)),
        ];
        let width_at = |zoom: f64, class_index: usize| {
            let (_, opts) = tile_options(zoom);
            let scene = build_scene(&style, &features, &opts);
            let fill_layer = scene
                .layers
                .iter()
                .find(|l| {
                    l.z_order as usize
                        == style
                            .layers
                            .iter()
                            .position(|x| x.id == "highway-fill")
                            .unwrap()
                })
                .unwrap();
            match &fill_layer.features[class_index] {
                RenderFeature::Stroke {
                    width,
                    width_next_zoom,
                    ..
                } => (*width, *width_next_zoom),
                other => panic!("{other:?}"),
            }
        };
        let (motorway12, next12) = width_at(12.0, 0);
        assert_eq!(motorway12, 6.0);
        assert!(next12 > motorway12, "the style widens roads towards z13");
        assert_eq!(width_at(12.0, 1).0, 1.5);
        assert!(width_at(10.0, 0).0 < motorway12 && motorway12 < width_at(15.0, 0).0);
    }

    #[test]
    fn dashes_scale_with_line_width() {
        let style = default_style();
        let (tile, opts) = tile_options(13.0);
        let scene = build_scene(
            &style,
            &[feature("boundary", "administrative", None, road(tile))],
            &opts,
        );
        let RenderFeature::Stroke { dash, width, .. } = &scene.layers[0].features[0] else {
            panic!()
        };
        assert_eq!(*width, 1.5);
        assert_eq!(dash, &vec![6.0, 3.0]);
        assert_eq!(
            dash_pattern(&[1.0, 2.0, 3.0], 2.0),
            vec![2.0, 4.0, 6.0, 2.0, 4.0, 6.0]
        );
        assert!(dash_pattern(&[0.0, 0.0], 2.0).is_empty());
        assert!(dash_pattern(&[-1.0, 2.0], 2.0).is_empty());
        // A pattern repeating every 0.02 px would cut millions of dashes.
        assert!(dash_pattern(&[0.01, 0.01], 1.0).is_empty());
        assert!(dash_pattern(&[1.0e-30, 1.0e-30], 1.0e10).is_empty());
        assert!(dash_pattern(&[3.0e38, 3.0e38], 10.0).is_empty(), "overflow");
        assert_eq!(dash_pattern(&[0.125], 2.0), vec![0.25, 0.25]);
    }

    #[test]
    fn polygon_outlines_stroke_every_ring_closed() {
        let style = default_style();
        let (tile, opts) = tile_options(14.0);
        let scene = build_scene(
            &style,
            &[feature("building", "yes", None, square(tile, true))],
            &opts,
        );
        let outlines: Vec<&Vec<[f32; 2]>> = scene
            .layers
            .iter()
            .flat_map(|l| &l.features)
            .filter_map(|f| match f {
                RenderFeature::Stroke { coords, .. } => Some(coords),
                _ => None,
            })
            .collect();
        assert_eq!(outlines.len(), 2);
        assert!(
            outlines
                .iter()
                .all(|r| r.first() == r.last() && r.len() == 5)
        );
    }

    #[test]
    fn multipolygons_become_one_fill_per_part() {
        let style = default_style();
        let (tile, opts) = tile_options(13.0);
        let Geometry::Polygon(a) = square(tile, false) else {
            panic!()
        };
        let Geometry::Polygon(b) = square(tile, true) else {
            panic!()
        };
        let g = Geometry::MultiPolygon(MultiPolygon(vec![a, b]));
        let scene = build_scene(&style, &[feature("water", "lake", None, g)], &opts);
        assert_eq!(fills(&scene).len(), 2);
    }

    #[test]
    fn points_become_circles_and_labels() {
        let style = default_style();
        let (tile, mut opts) = tile_options(16.0);
        opts.zoom = 16.0;
        let bb = tile.bbox();
        let at = Geometry::Point(Point::new(
            (bb.min_lon + bb.max_lon) / 2.0,
            (bb.min_lat + bb.max_lat) / 2.0,
        ));
        let scene = build_scene(
            &style,
            &[feature("shop", "bakery", Some("Bread & Co"), at)],
            &opts,
        );
        let all: Vec<&RenderFeature> = scene.layers.iter().flat_map(|l| &l.features).collect();
        let circle = all.iter().find_map(|f| match f {
            RenderFeature::Circle { center, radius, .. } => Some((*center, *radius)),
            _ => None,
        });
        let (center, radius) = circle.expect("a dot");
        assert!((center[0] - 256.0).abs() < 0.5 && (center[1] - 256.0).abs() < 0.5);
        assert_eq!(radius, 3.5);
        let label = all.iter().find_map(|f| match f {
            RenderFeature::Label(l) => Some(l),
            _ => None,
        });
        let label = label.expect("a label");
        assert_eq!(label.text, "Bread & Co");
        assert!(matches!(label.anchor, LabelAnchor::Point(_)));
        assert_eq!(label.style.font_size, 10.0);
    }

    #[test]
    fn unnamed_features_get_no_label() {
        let style = default_style();
        let (tile, mut opts) = tile_options(16.0);
        opts.zoom = 16.0;
        let bb = tile.bbox();
        let at = Geometry::Point(Point::new(bb.min_lon, bb.min_lat));
        let scene = build_scene(&style, &[feature("shop", "bakery", None, at)], &opts);
        assert!(
            scene
                .layers
                .iter()
                .flat_map(|l| &l.features)
                .all(|f| !matches!(f, RenderFeature::Label(_)))
        );
    }

    #[test]
    fn road_labels_follow_the_line_and_rank_by_layer_order() {
        let style = default_style();
        let (tile, opts) = tile_options(14.0);
        let bb = tile.bbox();
        let at = Geometry::Point(Point::new(bb.min_lon, bb.min_lat));
        let features = [
            feature("highway", "primary", Some("Main Street"), road(tile)),
            feature("place", "town", Some("Springfield"), at),
        ];
        let scene = build_scene(&style, &features, &opts);
        let labels: Vec<&LabelCandidate> = scene
            .layers
            .iter()
            .flat_map(|l| &l.features)
            .filter_map(|f| match f {
                RenderFeature::Label(l) => Some(l),
                _ => None,
            })
            .collect();
        let road_label = labels.iter().find(|l| l.text == "Main Street").unwrap();
        let place_label = labels.iter().find(|l| l.text == "Springfield").unwrap();
        assert!(matches!(road_label.anchor, LabelAnchor::Line(_)));
        assert_eq!(road_label.style.max_angle_degrees, 30.0);
        assert!(
            place_label.layer_rank < road_label.layer_rank,
            "places are placed first"
        );
        assert!(place_label.style.font_size > road_label.style.font_size);
    }

    #[test]
    fn culling_drops_off_screen_primitives() {
        let style = default_style();
        let (tile, mut opts) = tile_options(13.0);
        let features = [feature("landuse", "forest", None, square(tile, false))];
        opts.cull = Some([600.0, 600.0, 700.0, 700.0]);
        assert!(build_scene(&style, &features, &opts).layers.is_empty());
        opts.cull = Some([0.0, 0.0, 100.0, 100.0]);
        assert_eq!(build_scene(&style, &features, &opts).layers.len(), 1);
    }

    #[test]
    fn interior_point_lies_inside_a_concave_polygon() {
        // A "C" shape whose bounding-box centre is in the notch.
        let c = Polygon::new(
            LineString::from(vec![
                (0.0, 0.0),
                (10.0, 0.0),
                (10.0, 2.0),
                (2.0, 2.0),
                (2.0, 8.0),
                (10.0, 8.0),
                (10.0, 10.0),
                (0.0, 10.0),
                (0.0, 0.0),
            ]),
            vec![],
        );
        let mapping = PixelMapping::for_tile(TileCoord::new(0, 0, Zoom(0)));
        let pt = interior_point(&c, &mapping).unwrap();
        let rings = project_polygon(&c, &mapping).unwrap();
        // Ray-cast containment.
        let inside = {
            let ring = &rings[0];
            let mut inside = false;
            for i in 0..ring.len() {
                let (a, b) = (ring[i], ring[(i + 1) % ring.len()]);
                if (a[1] > pt[1]) != (b[1] > pt[1])
                    && pt[0] < a[0] + (pt[1] - a[1]) / (b[1] - a[1]) * (b[0] - a[0])
                {
                    inside = !inside;
                }
            }
            inside
        };
        assert!(inside, "{pt:?}");
    }

    #[test]
    fn symbol_font_placement_and_rotation_alignment_are_honoured() {
        let symbol = |id: &str, layout: serde_json::Value| {
            let mut layout = layout;
            layout["text-field"] = "{name}".into();
            serde_json::json!({"id": id, "type": "symbol", "source": "s",
                               "source-layer": "highway", "layout": layout})
        };
        let style = Style::from_value(&serde_json::json!({
            "version": 8,
            "sources": {"s": {"type": "vector", "tiles": ["x/{z}/{x}/{y}"]}},
            "layers": [
                symbol("along", serde_json::json!({"symbol-placement": "line",
                                                    "text-font": ["Noto Sans Bold"]})),
                symbol("centre", serde_json::json!({"symbol-placement": "line-center"})),
                symbol("flat", serde_json::json!({"symbol-placement": "line",
                                                  "text-rotation-alignment": "viewport"})),
                symbol("flat-centre", serde_json::json!({"symbol-placement": "line-center",
                                                         "text-rotation-alignment": "viewport"})),
                symbol("mapped-point", serde_json::json!({"text-rotation-alignment": "map"})),
            ],
        }))
        .unwrap();
        let (tile, opts) = tile_options(14.0);
        let f = feature("highway", "primary", Some("Main Street"), road(tile));
        let scene = build_scene(&style, &[f], &opts);
        let labels: Vec<&LabelCandidate> = scene
            .layers
            .iter()
            .flat_map(|l| &l.features)
            .filter_map(|f| match f {
                RenderFeature::Label(l) => Some(l),
                _ => None,
            })
            .collect();
        assert_eq!(labels.len(), 5);
        assert!(matches!(labels[0].anchor, LabelAnchor::Line(_)));
        assert_eq!(labels[0].style.font.names(), ["Noto Sans Bold"]);
        assert_eq!(
            labels[1].style.font.names(),
            ["Open Sans Regular", "Arial Unicode MS Regular"],
            "MapLibre's default stack"
        );
        assert!(matches!(labels[1].anchor, LabelAnchor::LineCenter(_)));
        // Viewport-aligned text along a line stays horizontal, at the
        // line's middle.
        for flat in &labels[2..4] {
            let LabelAnchor::Point(p) = flat.anchor else {
                panic!("{:?}", flat.anchor)
            };
            assert!(
                (p[0] - 256.0).abs() < 1.0 && (p[1] - 256.0).abs() < 1.0,
                "{p:?}"
            );
        }
        // Point placement is horizontal whatever the alignment (no bearing).
        assert!(matches!(labels[4].anchor, LabelAnchor::Point(_)));
    }

    #[test]
    fn backgrounds_paint_in_style_order() {
        let style = Style::from_value(&serde_json::json!({
            "version": 8,
            "sources": {"s": {"type": "vector", "tiles": ["x/{z}/{x}/{y}"]}},
            "layers": [
                {"id": "base", "type": "background", "paint": {"background-color": "#ff0000"}},
                {"id": "tint", "type": "background",
                 "paint": {"background-color": "#0000ff", "background-opacity": 0.5}},
                {"id": "water", "type": "fill", "source": "s", "source-layer": "water"},
                {"id": "veil", "type": "background",
                 "paint": {"background-color": "rgba(255, 255, 255, 0.5)"}},
            ],
        }))
        .unwrap();
        let (tile, opts) = tile_options(10.0);
        // The backgrounds below everything fold into the clear color...
        let scene = build_scene(&style, &[], &opts);
        assert_eq!(scene.background.to_rgba8(), [128, 0, 128, 255]);
        // ...and a later one paints over the layers before it, in order,
        // even with no cull rectangle (it then covers the whole world).
        assert_eq!(scene.layers.len(), 1);
        assert_eq!(scene.layers[0].z_order, 3);
        let RenderFeature::Fill { coords, color } = &scene.layers[0].features[0] else {
            panic!("{:?}", scene.layers[0])
        };
        assert_eq!(color.to_rgba8(), [255, 255, 255, 128]);
        let n = f32::from(1u16 << tile.z.0); // the mapping is the tile's
        let (x0, y0) = (coords[0][0][0], coords[0][0][1]);
        let (x1, y1) = (coords[0][2][0], coords[0][2][1]);
        assert!(x0 <= 0.0 && y0 <= 0.0 && x1 >= 512.0 && y1 >= 512.0);
        assert!(
            (x1 - x0 - 512.0 * n).abs() < 1.0,
            "the world is 2^z tiles wide"
        );
        // With data, the order is base+tint, water, veil.
        let water = feature("water", "lake", None, square(tile, false));
        let scene = build_scene(&style, &[water], &opts);
        let order: Vec<i32> = scene.layers.iter().map(|l| l.z_order).collect();
        assert_eq!(order, [2, 3]);
        // A tile scene's background covers exactly its clip.
        let scene = build_scene(&style, &[], &SceneOptions::for_tile(tile));
        let RenderFeature::Fill { coords, .. } = &scene.layers[0].features[0] else {
            panic!()
        };
        assert_eq!(
            coords[0],
            vec![[0.0, 0.0], [512.0, 0.0], [512.0, 512.0], [0.0, 512.0]]
        );
        assert_eq!(scene.layers[0].clip, Some([0.0, 0.0, 512.0, 512.0]));
    }

    #[test]
    fn numeric_and_boolean_filters_match_typed_attributes() {
        use osmic_tiles::mvt_decode::AttrValue;
        let style = Style::from_value(&serde_json::json!({
            "version": 8,
            "sources": {"s": {"type": "vector", "tiles": ["x/{z}/{x}/{y}"]}},
            "layers": [
                {"id": "legacy", "type": "line", "source": "s", "source-layer": "boundary",
                 "filter": ["==", "admin_level", 2]},
                {"id": "rank", "type": "line", "source": "s", "source-layer": "boundary",
                 "filter": ["<=", ["get", "rank"], 3]},
                {"id": "oneway", "type": "line", "source": "s", "source-layer": "boundary",
                 "filter": ["==", ["get", "oneway"], true]},
            ],
        }))
        .unwrap();
        let (tile, opts) = tile_options(10.0);
        let with = |tags: Vec<(&str, AttrValue)>| {
            let mut f = feature("boundary", "x", None, road(tile));
            f.tags = tags.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
            f
        };
        let features = [
            with(vec![
                ("admin_level", AttrValue::Int(2)),
                ("rank", AttrValue::UInt(3)),
                ("oneway", AttrValue::Bool(true)),
            ]),
            with(vec![
                ("admin_level", AttrValue::Int(4)),
                ("rank", AttrValue::Float(3.5)),
                ("oneway", AttrValue::Bool(false)),
            ]),
            // Strings are not numbers: MapLibre does not coerce them.
            with(vec![("admin_level", "2".into()), ("oneway", "true".into())]),
        ];
        let scene = build_scene(&style, &features, &opts);
        let counts: Vec<usize> = scene.layers.iter().map(|l| l.features.len()).collect();
        assert_eq!(counts, [1, 1, 1], "one typed feature matches each filter");
    }

    #[test]
    fn json_style_renders_the_same_scene() {
        // The default style and its serialised form are interchangeable.
        let a = default_style_json("pmtiles://x.pmtiles");
        let b = Style::from_json(&a.to_json()).unwrap();
        let (tile, opts) = tile_options(14.0);
        let features = [
            feature("landuse", "forest", None, square(tile, true)),
            feature("highway", "primary", Some("Main"), road(tile)),
        ];
        let sa = build_scene(&a, &features, &opts);
        let sb = build_scene(&b, &features, &opts);
        assert_eq!(format!("{sa:?}"), format!("{sb:?}"));
    }
}
