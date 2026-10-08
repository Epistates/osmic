//! The typed style model and its MapLibre JSON reader/writer.

use std::collections::HashSet;

use osmic_core::Color;
use serde_json::{Map, Value as Json};

use crate::error::StyleError;
use crate::expr::Expr;
use crate::property::{
    Alignment, LineCap, LineJoin, Property, PropertyValue, SymbolPlacement, TextAnchor,
    TextTransform, eval_or,
};
use crate::value::{EvalContext, number_json};

type JsonMap = Map<String, Json>;

/// A vector tile source.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct VectorSource {
    /// TileJSON or `pmtiles://` URL.
    pub url: Option<String>,
    /// Tile URL templates (`{z}/{x}/{y}`), used when `url` is absent.
    pub tiles: Vec<String>,
    pub min_zoom: Option<u8>,
    pub max_zoom: Option<u8>,
    /// Attribution shown by clients.
    pub attribution: Option<String>,
    /// `[west, south, east, north]`.
    pub bounds: Option<[f64; 4]>,
}

impl VectorSource {
    /// Parse a `"type": "vector"` source object.
    pub fn from_json(json: &Json, path: &str) -> Result<Self, StyleError> {
        let obj = json
            .as_object()
            .ok_or_else(|| StyleError::invalid(path, "a source must be an object"))?;
        let mut src = Self::default();
        for (key, value) in obj {
            let p = format!("{path}.{key}");
            let string = || {
                value
                    .as_str()
                    .map(str::to_string)
                    .ok_or_else(|| StyleError::invalid(&p, "expected a string"))
            };
            let zoom = || {
                value
                    .as_u64()
                    .and_then(|z| u8::try_from(z).ok())
                    .ok_or_else(|| StyleError::invalid(&p, "expected a zoom level"))
            };
            match key.as_str() {
                "type" => {
                    if value != "vector" {
                        let ty = value
                            .as_str()
                            .map_or_else(|| value.to_string(), str::to_string);
                        return Err(StyleError::unsupported(&p, "source type", ty));
                    }
                }
                "url" => src.url = Some(string()?),
                "attribution" => src.attribution = Some(string()?),
                "minzoom" => src.min_zoom = Some(zoom()?),
                "maxzoom" => src.max_zoom = Some(zoom()?),
                "tiles" => {
                    src.tiles = value
                        .as_array()
                        .and_then(|a| {
                            a.iter()
                                .map(|t| t.as_str().map(str::to_string))
                                .collect::<Option<Vec<_>>>()
                        })
                        .ok_or_else(|| StyleError::invalid(&p, "expected an array of strings"))?;
                }
                "bounds" => {
                    let b = value
                        .as_array()
                        .filter(|a| a.len() == 4)
                        .and_then(|a| a.iter().map(Json::as_f64).collect::<Option<Vec<_>>>())
                        .ok_or_else(|| StyleError::invalid(&p, "expected four numbers"))?;
                    src.bounds = Some([b[0], b[1], b[2], b[3]]);
                }
                "scheme" => {
                    if value != "xyz" {
                        let scheme = value
                            .as_str()
                            .map_or_else(|| value.to_string(), str::to_string);
                        return Err(StyleError::unsupported(&p, "tile scheme", scheme));
                    }
                }
                other => return Err(StyleError::unsupported(&p, "source property", other)),
            }
        }
        if src.url.is_none() && src.tiles.is_empty() {
            return Err(StyleError::invalid(
                path,
                "a vector source needs `url` or `tiles`",
            ));
        }
        Ok(src)
    }

    /// Serialise to a source object.
    pub fn to_json(&self) -> Json {
        let mut m = JsonMap::new();
        m.insert("type".into(), "vector".into());
        if let Some(url) = &self.url {
            m.insert("url".into(), url.clone().into());
        }
        if !self.tiles.is_empty() {
            m.insert("tiles".into(), self.tiles.clone().into());
        }
        if let Some(z) = self.min_zoom {
            m.insert("minzoom".into(), z.into());
        }
        if let Some(z) = self.max_zoom {
            m.insert("maxzoom".into(), z.into());
        }
        if let Some(a) = &self.attribution {
            m.insert("attribution".into(), a.clone().into());
        }
        if let Some(b) = self.bounds {
            m.insert(
                "bounds".into(),
                b.iter().map(|n| number_json(*n)).collect::<Vec<_>>().into(),
            );
        }
        Json::Object(m)
    }
}

/// `background` layer properties.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct BackgroundLayer {
    pub color: Option<Property<Color>>,
    pub opacity: Option<Property<f64>>,
}

/// `fill` layer properties.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FillLayer {
    pub color: Option<Property<Color>>,
    pub opacity: Option<Property<f64>>,
}

/// `line` layer properties.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LineLayer {
    pub cap: Option<Property<LineCap>>,
    pub join: Option<Property<LineJoin>>,
    pub color: Option<Property<Color>>,
    pub width: Option<Property<f64>>,
    pub opacity: Option<Property<f64>>,
    /// Dash lengths in multiples of the line width.
    pub dasharray: Option<Property<Vec<f64>>>,
}

/// `circle` layer properties.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CircleLayer {
    pub radius: Option<Property<f64>>,
    pub color: Option<Property<Color>>,
    pub opacity: Option<Property<f64>>,
    pub stroke_color: Option<Property<Color>>,
    pub stroke_width: Option<Property<f64>>,
}

/// `symbol` layer properties (text labels only; icons are unsupported).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SymbolLayer {
    pub placement: Option<Property<SymbolPlacement>>,
    pub sort_key: Option<Property<f64>>,
    pub text_field: Option<Property<String>>,
    pub text_font: Option<Property<Vec<String>>>,
    pub text_size: Option<Property<f64>>,
    pub text_transform: Option<Property<TextTransform>>,
    pub text_anchor: Option<Property<TextAnchor>>,
    /// Offset in ems.
    pub text_offset: Option<Property<Vec<f64>>>,
    pub text_padding: Option<Property<f64>>,
    pub text_allow_overlap: Option<Property<bool>>,
    pub text_max_angle: Option<Property<f64>>,
    pub text_rotation_alignment: Option<Property<Alignment>>,
    pub text_color: Option<Property<Color>>,
    pub text_halo_color: Option<Property<Color>>,
    pub text_halo_width: Option<Property<f64>>,
    pub text_opacity: Option<Property<f64>>,
}

/// A fill with its resolved paint.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FillStyle {
    /// Fill color with `fill-opacity` folded into alpha.
    pub color: Color,
}

/// A line with its resolved paint and layout.
#[derive(Debug, Clone, PartialEq)]
pub struct LineStyle {
    /// Line color with `line-opacity` folded into alpha.
    pub color: Color,
    /// Width in logical pixels.
    pub width: f32,
    pub cap: LineCap,
    pub join: LineJoin,
    /// Dash pattern in multiples of `width`; empty = solid.
    pub dasharray: Vec<f32>,
}

/// A circle with its resolved paint.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CircleStyle {
    /// Radius in logical pixels.
    pub radius: f32,
    pub color: Color,
    pub stroke_color: Color,
    pub stroke_width: f32,
}

/// A symbol (text label) with its resolved paint and layout.
#[derive(Debug, Clone, PartialEq)]
pub struct SymbolStyle {
    pub placement: SymbolPlacement,
    pub sort_key: f32,
    /// The label text, after `text-transform`.
    pub text: String,
    pub font: Vec<String>,
    /// Size in logical pixels.
    pub size: f32,
    pub anchor: TextAnchor,
    /// Offset in ems.
    pub offset: [f32; 2],
    pub padding: f32,
    pub allow_overlap: bool,
    pub max_angle_degrees: f32,
    pub rotation_alignment: Alignment,
    /// Text color with `text-opacity` folded into alpha.
    pub color: Color,
    /// Halo color with `text-opacity` folded into alpha.
    pub halo_color: Color,
    pub halo_width: f32,
}

fn opacity(p: &Option<Property<f64>>, ctx: &EvalContext<'_>) -> f32 {
    eval_or(p, ctx, 1.0).clamp(0.0, 1.0) as f32
}

impl BackgroundLayer {
    /// Background color with opacity folded in.
    pub fn resolve(&self, ctx: &EvalContext<'_>) -> Color {
        eval_or(&self.color, ctx, Color::BLACK).with_opacity(opacity(&self.opacity, ctx))
    }
}

impl FillLayer {
    pub fn resolve(&self, ctx: &EvalContext<'_>) -> FillStyle {
        FillStyle {
            color: eval_or(&self.color, ctx, Color::BLACK)
                .with_opacity(opacity(&self.opacity, ctx)),
        }
    }
}

impl LineLayer {
    pub fn resolve(&self, ctx: &EvalContext<'_>) -> LineStyle {
        LineStyle {
            color: eval_or(&self.color, ctx, Color::BLACK)
                .with_opacity(opacity(&self.opacity, ctx)),
            width: eval_or(&self.width, ctx, 1.0).max(0.0) as f32,
            cap: eval_or(&self.cap, ctx, LineCap::Butt),
            join: eval_or(&self.join, ctx, LineJoin::Miter),
            dasharray: eval_or(&self.dasharray, ctx, Vec::new())
                .into_iter()
                .map(|d| d as f32)
                .collect(),
        }
    }
}

impl CircleLayer {
    pub fn resolve(&self, ctx: &EvalContext<'_>) -> CircleStyle {
        let op = opacity(&self.opacity, ctx);
        CircleStyle {
            radius: eval_or(&self.radius, ctx, 5.0).max(0.0) as f32,
            color: eval_or(&self.color, ctx, Color::BLACK).with_opacity(op),
            stroke_color: eval_or(&self.stroke_color, ctx, Color::BLACK).with_opacity(op),
            stroke_width: eval_or(&self.stroke_width, ctx, 0.0).max(0.0) as f32,
        }
    }
}

impl SymbolLayer {
    pub fn resolve(&self, ctx: &EvalContext<'_>) -> SymbolStyle {
        let op = opacity(&self.text_opacity, ctx);
        let offset = eval_or(&self.text_offset, ctx, vec![0.0, 0.0]);
        SymbolStyle {
            placement: eval_or(&self.placement, ctx, SymbolPlacement::Point),
            sort_key: eval_or(&self.sort_key, ctx, 0.0) as f32,
            text: eval_or(&self.text_transform, ctx, TextTransform::None).apply(&eval_or(
                &self.text_field,
                ctx,
                String::new(),
            )),
            font: eval_or(
                &self.text_font,
                ctx,
                vec![
                    "Open Sans Regular".into(),
                    "Arial Unicode MS Regular".into(),
                ],
            ),
            size: eval_or(&self.text_size, ctx, 16.0).max(0.0) as f32,
            anchor: eval_or(&self.text_anchor, ctx, TextAnchor::Center),
            offset: [
                offset.first().copied().unwrap_or(0.0) as f32,
                offset.get(1).copied().unwrap_or(0.0) as f32,
            ],
            padding: eval_or(&self.text_padding, ctx, 2.0).max(0.0) as f32,
            allow_overlap: eval_or(&self.text_allow_overlap, ctx, false),
            max_angle_degrees: eval_or(&self.text_max_angle, ctx, 45.0) as f32,
            rotation_alignment: eval_or(&self.text_rotation_alignment, ctx, Alignment::Auto),
            color: eval_or(&self.text_color, ctx, Color::BLACK).with_opacity(op),
            halo_color: eval_or(&self.text_halo_color, ctx, Color::TRANSPARENT).with_opacity(op),
            halo_width: eval_or(&self.text_halo_width, ctx, 0.0).max(0.0) as f32,
        }
    }
}

/// What a layer draws.
// A style holds tens of layers, so the size of the symbol variant is
// immaterial; boxing it would only make construction clumsier.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum LayerKind {
    Background(BackgroundLayer),
    Fill(FillLayer),
    Line(LineLayer),
    Circle(CircleLayer),
    Symbol(SymbolLayer),
}

impl LayerKind {
    /// The style-spec `type` string.
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Background(_) => "background",
            Self::Fill(_) => "fill",
            Self::Line(_) => "line",
            Self::Circle(_) => "circle",
            Self::Symbol(_) => "symbol",
        }
    }
}

/// Whether any of the given properties satisfies `pred`.
macro_rules! any_property {
    ($pred:ident; $($p:expr),+ $(,)?) => {
        false $(|| $p.as_ref().is_some_and(|p| p.$pred()))+
    };
}

impl LayerKind {
    /// Whether any property varies between features (reads attributes), so
    /// the resolved style must be evaluated per feature.
    pub fn is_data_driven(&self) -> bool {
        match self {
            Self::Background(l) => any_property!(depends_on_feature; l.color, l.opacity),
            Self::Fill(l) => any_property!(depends_on_feature; l.color, l.opacity),
            Self::Line(l) => {
                any_property!(depends_on_feature; l.cap, l.join, l.color, l.width, l.opacity, l.dasharray)
            }
            Self::Circle(l) => {
                any_property!(depends_on_feature; l.radius, l.color, l.opacity, l.stroke_color, l.stroke_width)
            }
            Self::Symbol(l) => any_property!(
                depends_on_feature; l.placement, l.sort_key, l.text_field, l.text_font, l.text_size,
                l.text_transform, l.text_anchor, l.text_offset, l.text_padding, l.text_allow_overlap,
                l.text_max_angle, l.text_rotation_alignment, l.text_color, l.text_halo_color,
                l.text_halo_width, l.text_opacity
            ),
        }
    }

    /// Whether the size of a line or circle (its width or radius) changes
    /// with zoom, so renderers that interpolate between integer zooms need
    /// the value at two zoom levels.
    pub fn size_depends_on_zoom(&self) -> bool {
        match self {
            Self::Line(l) => any_property!(depends_on_zoom; l.width),
            Self::Circle(l) => any_property!(depends_on_zoom; l.radius, l.stroke_width),
            _ => false,
        }
    }
}

/// One style layer.
#[derive(Debug, Clone, PartialEq)]
pub struct Layer {
    pub id: String,
    /// Source id (absent for `background`).
    pub source: Option<String>,
    /// Vector-tile layer within the source.
    pub source_layer: Option<String>,
    /// Inclusive lower zoom bound.
    pub min_zoom: Option<f64>,
    /// Exclusive upper zoom bound.
    pub max_zoom: Option<f64>,
    pub filter: Option<Expr>,
    /// `layout.visibility`.
    pub visible: bool,
    /// Opaque editor metadata, preserved verbatim.
    pub metadata: Option<Json>,
    pub kind: LayerKind,
}

impl Layer {
    /// A visible layer with no zoom range or filter.
    pub fn new(id: impl Into<String>, kind: LayerKind) -> Self {
        Self {
            id: id.into(),
            source: None,
            source_layer: None,
            min_zoom: None,
            max_zoom: None,
            filter: None,
            visible: true,
            metadata: None,
            kind,
        }
    }

    /// Bind the layer to a vector-tile layer of `source`.
    pub fn from_source(mut self, source: &str, source_layer: &str) -> Self {
        self.source = Some(source.to_string());
        self.source_layer = Some(source_layer.to_string());
        self
    }

    /// Restrict the layer to `[min, max)` zoom levels.
    pub fn zoom_range(mut self, min: Option<f64>, max: Option<f64>) -> Self {
        self.min_zoom = min;
        self.max_zoom = max;
        self
    }

    /// Set the filter.
    pub fn with_filter(mut self, filter: Expr) -> Self {
        self.filter = Some(filter);
        self
    }

    /// Whether the layer draws at `zoom` (visibility and zoom range).
    pub fn is_active_at(&self, zoom: f64) -> bool {
        self.visible
            && self.min_zoom.is_none_or(|min| zoom >= min)
            && self.max_zoom.is_none_or(|max| zoom < max)
    }

    /// Whether the filter accepts the feature in `ctx`.
    pub fn accepts(&self, ctx: &EvalContext<'_>) -> bool {
        self.filter.as_ref().is_none_or(|f| f.evaluate_bool(ctx))
    }
}

/// A style document: sources plus an ordered list of layers.
///
/// `sources` is kept as the raw JSON object so callers (for example the tile
/// server) can rewrite it freely; it is validated on parse and exposed typed
/// through [`Style::vector_source`].
#[derive(Debug, Clone, PartialEq)]
pub struct Style {
    pub version: u8,
    pub name: String,
    /// Glyph (SDF font) URL template used by MapLibre clients for text.
    pub glyphs: Option<String>,
    /// Initial `[lon, lat]`.
    pub center: Option<[f64; 2]>,
    /// Initial zoom.
    pub zoom: Option<f64>,
    pub metadata: Option<Json>,
    pub sources: Json,
    pub layers: Vec<Layer>,
}

const UNSUPPORTED_LAYER_TYPES: &[&str] = &[
    "raster",
    "hillshade",
    "color-relief",
    "fill-extrusion",
    "heatmap",
    "sky",
    "model",
];

impl Style {
    /// An empty version-8 style.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            version: 8,
            name: name.into(),
            glyphs: None,
            center: None,
            zoom: None,
            metadata: None,
            sources: Json::Object(JsonMap::new()),
            layers: Vec::new(),
        }
    }

    /// Parse a style from JSON text.
    pub fn from_json(text: &str) -> Result<Self, StyleError> {
        let json: Json = serde_json::from_str(text).map_err(|e| StyleError::Json(e.to_string()))?;
        Self::from_value(&json)
    }

    /// Parse a style from a JSON value.
    pub fn from_value(json: &Json) -> Result<Self, StyleError> {
        let obj = json
            .as_object()
            .ok_or_else(|| StyleError::invalid("style", "a style must be a JSON object"))?;
        let mut style = Style::new("");
        let mut layers_json: &[Json] = &[];
        let mut saw_version = false;
        for (key, value) in obj {
            let path = key.as_str();
            match path {
                "version" => {
                    saw_version = true;
                    if value != 8 {
                        return Err(StyleError::invalid(
                            path,
                            "only style version 8 is supported",
                        ));
                    }
                }
                "name" => {
                    style.name = value
                        .as_str()
                        .ok_or_else(|| StyleError::invalid(path, "expected a string"))?
                        .to_string();
                }
                "glyphs" => {
                    style.glyphs = Some(
                        value
                            .as_str()
                            .ok_or_else(|| StyleError::invalid(path, "expected a string"))?
                            .to_string(),
                    );
                }
                "metadata" => style.metadata = Some(value.clone()),
                "center" => {
                    let c = value
                        .as_array()
                        .filter(|a| a.len() == 2)
                        .and_then(|a| a.iter().map(Json::as_f64).collect::<Option<Vec<_>>>())
                        .ok_or_else(|| StyleError::invalid(path, "expected [lon, lat]"))?;
                    style.center = Some([c[0], c[1]]);
                }
                "zoom" => {
                    style.zoom = Some(
                        value
                            .as_f64()
                            .ok_or_else(|| StyleError::invalid(path, "expected a number"))?,
                    );
                }
                "bearing" | "pitch" => {
                    if value.as_f64() != Some(0.0) {
                        return Err(StyleError::unsupported(
                            path,
                            "style property",
                            format!("non-zero {path}"),
                        ));
                    }
                }
                "sources" => {
                    let sources = value
                        .as_object()
                        .ok_or_else(|| StyleError::invalid(path, "expected an object"))?;
                    for (id, src) in sources {
                        VectorSource::from_json(src, &format!("sources.{id}"))?;
                    }
                    style.sources = value.clone();
                }
                "layers" => {
                    layers_json = value
                        .as_array()
                        .ok_or_else(|| StyleError::invalid(path, "expected an array"))?;
                }
                other => return Err(StyleError::unsupported(other, "style property", other)),
            }
        }
        if !saw_version {
            return Err(StyleError::invalid("version", "missing"));
        }
        let mut ids = HashSet::new();
        for (i, layer_json) in layers_json.iter().enumerate() {
            let layer = Layer::from_json(layer_json, &format!("layers[{i}]"))?;
            if !ids.insert(layer.id.clone()) {
                return Err(StyleError::invalid(
                    &format!("layers[{i}].id"),
                    format!("duplicate layer id `{}`", layer.id),
                ));
            }
            if let Some(source) = &layer.source
                && style.sources.get(source).is_none()
            {
                return Err(StyleError::invalid(
                    &format!("layers[{i}].source"),
                    format!("unknown source `{source}`"),
                ));
            }
            style.layers.push(layer);
        }
        Ok(style)
    }

    /// Serialise to a JSON value.
    pub fn to_value(&self) -> Json {
        let mut m = JsonMap::new();
        m.insert("version".into(), self.version.into());
        if !self.name.is_empty() {
            m.insert("name".into(), self.name.clone().into());
        }
        if let Some(md) = &self.metadata {
            m.insert("metadata".into(), md.clone());
        }
        if let Some([lon, lat]) = self.center {
            m.insert(
                "center".into(),
                vec![number_json(lon), number_json(lat)].into(),
            );
        }
        if let Some(z) = self.zoom {
            m.insert("zoom".into(), number_json(z));
        }
        if let Some(g) = &self.glyphs {
            m.insert("glyphs".into(), g.clone().into());
        }
        m.insert("sources".into(), self.sources.clone());
        m.insert(
            "layers".into(),
            self.layers
                .iter()
                .map(Layer::to_json)
                .collect::<Vec<_>>()
                .into(),
        );
        Json::Object(m)
    }

    /// Serialise to pretty-printed MapLibre style JSON.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(&self.to_value()).expect("a JSON value always serialises")
    }

    /// The typed vector source `id`, if present and valid.
    pub fn vector_source(&self, id: &str) -> Option<VectorSource> {
        VectorSource::from_json(self.sources.get(id)?, id).ok()
    }

    /// Insert or replace the vector source `id`.
    pub fn set_vector_source(&mut self, id: &str, source: &VectorSource) {
        if !self.sources.is_object() {
            self.sources = Json::Object(JsonMap::new());
        }
        if let Some(map) = self.sources.as_object_mut() {
            map.insert(id.to_string(), source.to_json());
        }
    }

    /// The layer with the given id.
    pub fn layer(&self, id: &str) -> Option<&Layer> {
        self.layers.iter().find(|l| l.id == id)
    }
}

impl Layer {
    /// Parse one layer object found at `path`.
    pub fn from_json(json: &Json, path: &str) -> Result<Self, StyleError> {
        let obj = json
            .as_object()
            .ok_or_else(|| StyleError::invalid(path, "a layer must be an object"))?;
        let get_str = |key: &str| -> Result<Option<String>, StyleError> {
            obj.get(key)
                .map(|v| {
                    v.as_str().map(str::to_string).ok_or_else(|| {
                        StyleError::invalid(&format!("{path}.{key}"), "expected a string")
                    })
                })
                .transpose()
        };
        let get_num = |key: &str| -> Result<Option<f64>, StyleError> {
            obj.get(key)
                .map(|v| {
                    v.as_f64().ok_or_else(|| {
                        StyleError::invalid(&format!("{path}.{key}"), "expected a number")
                    })
                })
                .transpose()
        };
        let id =
            get_str("id")?.ok_or_else(|| StyleError::invalid(&format!("{path}.id"), "missing"))?;
        let ty = get_str("type")?
            .ok_or_else(|| StyleError::invalid(&format!("{path}.type"), "missing"))?;
        for key in obj.keys() {
            if !matches!(
                key.as_str(),
                "id" | "type"
                    | "source"
                    | "source-layer"
                    | "minzoom"
                    | "maxzoom"
                    | "filter"
                    | "layout"
                    | "paint"
                    | "metadata"
            ) {
                return Err(StyleError::unsupported(
                    &format!("{path}.{key}"),
                    "layer property",
                    key,
                ));
            }
        }
        let section = |key: &str| -> Result<Option<&JsonMap>, StyleError> {
            obj.get(key)
                .map(|v| {
                    v.as_object().ok_or_else(|| {
                        StyleError::invalid(&format!("{path}.{key}"), "expected an object")
                    })
                })
                .transpose()
        };
        let (layout, paint) = (section("layout")?, section("paint")?);
        let (kind, visible) = parse_kind(&ty, layout, paint, path)?;

        let filter = obj
            .get("filter")
            .map(|f| Expr::parse_filter(f, &format!("{path}.filter")))
            .transpose()?;
        let source = get_str("source")?;
        let source_layer = get_str("source-layer")?;
        if !matches!(kind, LayerKind::Background(_)) {
            if source.is_none() {
                return Err(StyleError::invalid(&format!("{path}.source"), "missing"));
            }
            if source_layer.is_none() {
                return Err(StyleError::invalid(
                    &format!("{path}.source-layer"),
                    "missing (required for vector sources)",
                ));
            }
        }
        Ok(Self {
            id,
            source,
            source_layer,
            min_zoom: get_num("minzoom")?,
            max_zoom: get_num("maxzoom")?,
            filter,
            visible,
            metadata: obj.get("metadata").cloned(),
            kind,
        })
    }

    /// Serialise to a layer object.
    pub fn to_json(&self) -> Json {
        let mut m = JsonMap::new();
        m.insert("id".into(), self.id.clone().into());
        m.insert("type".into(), self.kind.type_name().into());
        if let Some(s) = &self.source {
            m.insert("source".into(), s.clone().into());
        }
        if let Some(s) = &self.source_layer {
            m.insert("source-layer".into(), s.clone().into());
        }
        if let Some(z) = self.min_zoom {
            m.insert("minzoom".into(), number_json(z));
        }
        if let Some(z) = self.max_zoom {
            m.insert("maxzoom".into(), number_json(z));
        }
        if let Some(f) = &self.filter {
            m.insert("filter".into(), f.to_json());
        }
        if let Some(md) = &self.metadata {
            m.insert("metadata".into(), md.clone());
        }
        let (mut layout, mut paint) = (JsonMap::new(), JsonMap::new());
        if !self.visible {
            layout.insert("visibility".into(), "none".into());
        }
        write_props(&self.kind, &mut layout, &mut paint);
        if !layout.is_empty() {
            m.insert("layout".into(), Json::Object(layout));
        }
        if !paint.is_empty() {
            m.insert("paint".into(), Json::Object(paint));
        }
        Json::Object(m)
    }
}

fn put<T: PropertyValue>(map: &mut JsonMap, key: &str, p: &Option<Property<T>>) {
    if let Some(p) = p {
        map.insert(key.to_string(), p.to_json());
    }
}

fn write_props(kind: &LayerKind, layout: &mut JsonMap, paint: &mut JsonMap) {
    match kind {
        LayerKind::Background(b) => {
            put(paint, "background-color", &b.color);
            put(paint, "background-opacity", &b.opacity);
        }
        LayerKind::Fill(f) => {
            put(paint, "fill-color", &f.color);
            put(paint, "fill-opacity", &f.opacity);
        }
        LayerKind::Line(l) => {
            put(layout, "line-cap", &l.cap);
            put(layout, "line-join", &l.join);
            put(paint, "line-color", &l.color);
            put(paint, "line-width", &l.width);
            put(paint, "line-opacity", &l.opacity);
            put(paint, "line-dasharray", &l.dasharray);
        }
        LayerKind::Circle(c) => {
            put(paint, "circle-radius", &c.radius);
            put(paint, "circle-color", &c.color);
            put(paint, "circle-opacity", &c.opacity);
            put(paint, "circle-stroke-color", &c.stroke_color);
            put(paint, "circle-stroke-width", &c.stroke_width);
        }
        LayerKind::Symbol(s) => {
            put(layout, "symbol-placement", &s.placement);
            put(layout, "symbol-sort-key", &s.sort_key);
            put(layout, "text-field", &s.text_field);
            put(layout, "text-font", &s.text_font);
            put(layout, "text-size", &s.text_size);
            put(layout, "text-transform", &s.text_transform);
            put(layout, "text-anchor", &s.text_anchor);
            put(layout, "text-offset", &s.text_offset);
            put(layout, "text-padding", &s.text_padding);
            put(layout, "text-allow-overlap", &s.text_allow_overlap);
            put(layout, "text-max-angle", &s.text_max_angle);
            put(
                layout,
                "text-rotation-alignment",
                &s.text_rotation_alignment,
            );
            put(paint, "text-color", &s.text_color);
            put(paint, "text-halo-color", &s.text_halo_color);
            put(paint, "text-halo-width", &s.text_halo_width);
            put(paint, "text-opacity", &s.text_opacity);
        }
    }
}

fn prop<T: PropertyValue>(value: &Json, path: &str) -> Result<Option<Property<T>>, StyleError> {
    Property::parse(value, path).map(Some)
}

/// `text-field`: an expression or a literal, with the legacy `{name}` token
/// shorthand for a single whole-string attribute reference.
fn text_field(value: &Json, path: &str) -> Result<Option<Property<String>>, StyleError> {
    if let Some(s) = value.as_str()
        && s.contains(['{', '}'])
    {
        let key = s
            .strip_prefix('{')
            .and_then(|r| r.strip_suffix('}'))
            .filter(|k| !k.is_empty() && !k.contains(['{', '}']));
        return match key {
            Some(k) => Ok(Some(Property::Expr(Expr::get(k)))),
            None => Err(StyleError::unsupported(path, "text-field syntax", s)),
        };
    }
    prop(value, path)
}

fn parse_kind(
    ty: &str,
    layout: Option<&JsonMap>,
    paint: Option<&JsonMap>,
    path: &str,
) -> Result<(LayerKind, bool), StyleError> {
    let mut visible = true;
    let empty = JsonMap::new();
    let (layout, paint) = (layout.unwrap_or(&empty), paint.unwrap_or(&empty));
    let unsupported = |section: &str, key: &str| {
        StyleError::unsupported(&format!("{path}.{section}.{key}"), "property", key)
    };

    // `visibility` is common to every layout section.
    let mut layout_props: Vec<(&String, &Json)> = Vec::new();
    for (key, value) in layout {
        if key == "visibility" {
            visible = match value.as_str() {
                Some("visible") => true,
                Some("none") => false,
                _ => {
                    return Err(StyleError::invalid(
                        &format!("{path}.layout.visibility"),
                        "expected `visible` or `none`",
                    ));
                }
            };
        } else {
            layout_props.push((key, value));
        }
    }
    let lp = |key: &str| format!("{path}.layout.{key}");
    let pp = |key: &str| format!("{path}.paint.{key}");

    let kind = match ty {
        "background" => {
            let mut l = BackgroundLayer::default();
            if let Some((k, _)) = layout_props.first() {
                return Err(unsupported("layout", k));
            }
            for (k, v) in paint {
                match k.as_str() {
                    "background-color" => l.color = prop(v, &pp(k))?,
                    "background-opacity" => l.opacity = prop(v, &pp(k))?,
                    _ => return Err(unsupported("paint", k)),
                }
            }
            LayerKind::Background(l)
        }
        "fill" => {
            let mut l = FillLayer::default();
            if let Some((k, _)) = layout_props.first() {
                return Err(unsupported("layout", k));
            }
            for (k, v) in paint {
                match k.as_str() {
                    "fill-color" => l.color = prop(v, &pp(k))?,
                    "fill-opacity" => l.opacity = prop(v, &pp(k))?,
                    _ => return Err(unsupported("paint", k)),
                }
            }
            LayerKind::Fill(l)
        }
        "line" => {
            let mut l = LineLayer::default();
            for (k, v) in &layout_props {
                match k.as_str() {
                    "line-cap" => l.cap = prop(v, &lp(k))?,
                    "line-join" => l.join = prop(v, &lp(k))?,
                    _ => return Err(unsupported("layout", k)),
                }
            }
            for (k, v) in paint {
                match k.as_str() {
                    "line-color" => l.color = prop(v, &pp(k))?,
                    "line-width" => l.width = prop(v, &pp(k))?,
                    "line-opacity" => l.opacity = prop(v, &pp(k))?,
                    "line-dasharray" => l.dasharray = prop(v, &pp(k))?,
                    _ => return Err(unsupported("paint", k)),
                }
            }
            LayerKind::Line(l)
        }
        "circle" => {
            let mut l = CircleLayer::default();
            if let Some((k, _)) = layout_props.first() {
                return Err(unsupported("layout", k));
            }
            for (k, v) in paint {
                match k.as_str() {
                    "circle-radius" => l.radius = prop(v, &pp(k))?,
                    "circle-color" => l.color = prop(v, &pp(k))?,
                    "circle-opacity" => l.opacity = prop(v, &pp(k))?,
                    "circle-stroke-color" => l.stroke_color = prop(v, &pp(k))?,
                    "circle-stroke-width" => l.stroke_width = prop(v, &pp(k))?,
                    _ => return Err(unsupported("paint", k)),
                }
            }
            LayerKind::Circle(l)
        }
        "symbol" => {
            let mut l = SymbolLayer::default();
            for (k, v) in &layout_props {
                let p = lp(k);
                match k.as_str() {
                    "symbol-placement" => l.placement = prop(v, &p)?,
                    "symbol-sort-key" => l.sort_key = prop(v, &p)?,
                    "text-field" => l.text_field = text_field(v, &p)?,
                    "text-font" => l.text_font = prop(v, &p)?,
                    "text-size" => l.text_size = prop(v, &p)?,
                    "text-transform" => l.text_transform = prop(v, &p)?,
                    "text-anchor" => l.text_anchor = prop(v, &p)?,
                    "text-offset" => l.text_offset = prop(v, &p)?,
                    "text-padding" => l.text_padding = prop(v, &p)?,
                    "text-allow-overlap" => l.text_allow_overlap = prop(v, &p)?,
                    "text-max-angle" => l.text_max_angle = prop(v, &p)?,
                    "text-rotation-alignment" => l.text_rotation_alignment = prop(v, &p)?,
                    _ => return Err(unsupported("layout", k)),
                }
            }
            for (k, v) in paint {
                let p = pp(k);
                match k.as_str() {
                    "text-color" => l.text_color = prop(v, &p)?,
                    "text-halo-color" => l.text_halo_color = prop(v, &p)?,
                    "text-halo-width" => l.text_halo_width = prop(v, &p)?,
                    "text-opacity" => l.text_opacity = prop(v, &p)?,
                    _ => return Err(unsupported("paint", k)),
                }
            }
            LayerKind::Symbol(l)
        }
        other if UNSUPPORTED_LAYER_TYPES.contains(&other) => {
            return Err(StyleError::unsupported(
                &format!("{path}.type"),
                "layer type",
                other,
            ));
        }
        other => {
            return Err(StyleError::invalid(
                &format!("{path}.type"),
                format!("unknown layer type `{other}`"),
            ));
        }
    };
    Ok((kind, visible))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn doc(layers: Json) -> Json {
        json!({
            "version": 8,
            "sources": {"s": {"type": "vector", "tiles": ["http://x/{z}/{x}/{y}.mvt"]}},
            "layers": layers,
        })
    }

    fn layer(extra: Json) -> Json {
        let mut base = json!({"id": "l", "source": "s", "source-layer": "highway"});
        base.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        base
    }

    #[test]
    fn parses_a_maplibre_document_with_legacy_filter_and_token_text() {
        let style = Style::from_value(&doc(json!([
            {"id": "bg", "type": "background", "paint": {"background-color": "hsl(30, 40%, 95%)"}},
            layer(json!({
                "type": "line",
                "filter": ["all", ["==", "class", "primary"], ["!has", "tunnel"]],
                "layout": {"line-cap": "round"},
                "paint": {"line-color": "rgb(255, 0, 0)", "line-width": ["interpolate", ["linear"], ["zoom"], 5, 1, 15, 5], "line-dasharray": [2, 1]},
            })),
            layer(json!({
                "id": "t", "type": "symbol",
                "layout": {"text-field": "{name}", "symbol-placement": "line", "text-size": 12},
                "paint": {"text-color": "#333", "text-halo-width": 1.5},
            })),
        ])))
        .unwrap();
        assert_eq!(style.layers.len(), 3);
        let LayerKind::Line(line) = &style.layers[1].kind else {
            panic!()
        };
        let props = [("class", "primary")];
        let s = line.resolve(&EvalContext::new(10.0, &props));
        assert_eq!(s.width, 3.0);
        assert_eq!(s.cap, LineCap::Round);
        assert_eq!(s.dasharray, vec![2.0, 1.0]);
        assert_eq!(s.color.to_rgba8(), [255, 0, 0, 255]);
        assert!(style.layers[1].accepts(&EvalContext::new(10.0, &props)));
        let tunnel = [("class", "primary"), ("tunnel", "yes")];
        assert!(!style.layers[1].accepts(&EvalContext::new(10.0, &tunnel)));

        let LayerKind::Symbol(sym) = &style.layers[2].kind else {
            panic!()
        };
        let named = [("name", "Main St")];
        let r = sym.resolve(&EvalContext::new(10.0, &named));
        assert_eq!(r.text, "Main St");
        assert_eq!(r.placement, SymbolPlacement::Line);
        assert_eq!(r.size, 12.0);

        // Parsing then writing then parsing again is stable.
        let again = Style::from_json(&style.to_json()).unwrap();
        assert_eq!(again, style);
    }

    #[test]
    fn unsupported_constructs_are_errors_naming_the_construct() {
        let cases = [
            (
                json!([{"id": "r", "type": "raster", "source": "s"}]),
                "raster",
            ),
            (
                json!([{"id": "e", "type": "fill-extrusion", "source": "s", "source-layer": "building"}]),
                "fill-extrusion",
            ),
            (
                layer(json!({"type": "fill", "paint": {"fill-pattern": "x"}})),
                "fill-pattern",
            ),
            (
                layer(json!({"type": "line", "paint": {"line-gradient": []}})),
                "line-gradient",
            ),
            (
                layer(json!({"type": "line", "layout": {"line-miter-limit": 2}})),
                "line-miter-limit",
            ),
            (
                layer(json!({"type": "symbol", "layout": {"icon-image": "x"}})),
                "icon-image",
            ),
            (
                layer(json!({"type": "fill", "paint": {"fill-color": ["concat", "a", "b"]}})),
                "concat",
            ),
            (
                layer(json!({"type": "fill", "filter": ["==", "$type", "Polygon"]})),
                "$type",
            ),
            (
                layer(json!({"type": "symbol", "layout": {"text-field": "{a} {b}"}})),
                "{a} {b}",
            ),
            (layer(json!({"type": "fill", "ref": "other"})), "ref"),
        ];
        for (layers, construct) in cases {
            let layers = if layers.is_array() {
                layers
            } else {
                json!([layers])
            };
            let err = Style::from_value(&doc(layers)).expect_err(construct);
            assert_eq!(err.construct(), Some(construct), "{err}");
            assert!(err.to_string().contains(construct), "{err}");
        }
    }

    #[test]
    fn unsupported_source_and_style_properties_are_errors() {
        let mut d = doc(json!([]));
        d["sources"]["s"] = json!({"type": "raster", "tiles": ["x"]});
        assert_eq!(
            Style::from_value(&d).unwrap_err().construct(),
            Some("raster")
        );
        let mut d = doc(json!([]));
        d["terrain"] = json!({"source": "dem"});
        assert_eq!(
            Style::from_value(&d).unwrap_err().construct(),
            Some("terrain")
        );
        let mut d = doc(json!([]));
        d["pitch"] = json!(30);
        assert!(Style::from_value(&d).is_err());
    }

    #[test]
    fn invalid_colors_report_their_path() {
        let err = Style::from_value(&doc(json!([layer(json!({
            "type": "fill", "paint": {"fill-color": "bluish"}
        }))])))
        .unwrap_err();
        let text = err.to_string();
        assert!(
            text.contains("layers[0].paint.fill-color") && text.contains("bluish"),
            "{text}"
        );
    }

    #[test]
    fn structural_errors() {
        assert!(Style::from_json("not json").is_err());
        assert!(Style::from_value(&json!({"layers": []})).is_err()); // no version
        let mut d = doc(json!([
            layer(json!({"type": "fill"})),
            layer(json!({"type": "fill"}))
        ]));
        assert!(
            Style::from_value(&d)
                .unwrap_err()
                .to_string()
                .contains("duplicate")
        );
        d["layers"] = json!([{"id": "a", "type": "fill", "source": "nope", "source-layer": "x"}]);
        assert!(
            Style::from_value(&d)
                .unwrap_err()
                .to_string()
                .contains("unknown source")
        );
        d["layers"] = json!([{"id": "a", "type": "fill", "source": "s"}]);
        assert!(
            Style::from_value(&d)
                .unwrap_err()
                .to_string()
                .contains("source-layer")
        );
    }

    #[test]
    fn zoom_range_and_visibility() {
        let l = Layer::new("x", LayerKind::Fill(FillLayer::default()))
            .zoom_range(Some(5.0), Some(10.0));
        assert!(
            !l.is_active_at(4.9)
                && l.is_active_at(5.0)
                && l.is_active_at(9.9)
                && !l.is_active_at(10.0)
        );
        let style = Style::from_value(&doc(json!([layer(json!({
            "type": "fill", "layout": {"visibility": "none"}
        }))])))
        .unwrap();
        assert!(!style.layers[0].is_active_at(5.0));
        assert_eq!(Style::from_json(&style.to_json()).unwrap(), style);
    }
}
