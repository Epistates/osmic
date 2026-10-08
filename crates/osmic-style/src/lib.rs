//! A typed map style model for osmic.
//!
//! The crate reads and writes a **subset of the MapLibre style
//! specification** (version 8) and is the single source of truth for how
//! osmic maps look: the software renderer, the interactive viewer and
//! MapLibre clients all consume the style defined here.
//!
//! # Supported subset
//!
//! * **Sources**: `vector` (`url`, `tiles`, `minzoom`, `maxzoom`,
//!   `attribution`, `bounds`, `scheme: xyz`).
//! * **Layers**: `background`, `fill`, `line`, `circle` and `symbol` (text
//!   labels; no icons). `minzoom`, `maxzoom`, `filter`, `layout.visibility`.
//! * **Properties**: `background-color/opacity`, `fill-color/opacity`,
//!   `line-cap/join/color/width/opacity/dasharray`,
//!   `circle-radius/color/opacity/stroke-color/stroke-width/stroke-opacity`,
//!   `symbol-placement` (`point`, `line`, `line-center`), `symbol-sort-key`,
//!   `text-field/font/size/transform/anchor/offset/padding/allow-overlap/
//!   max-angle/rotation-alignment`, `text-color/halo-color/halo-width/opacity`.
//! * **Expressions**: see [`Expr`]. Layer filters may also use the legacy
//!   filter syntax.
//!
//! Anything else — other layer types, other properties, other expression
//! operators — fails to parse with a [`StyleError::Unsupported`] naming the
//! construct and its JSON path; nothing is silently ignored.
//!
//! # Default style
//!
//! [`default_style_json`] builds the osmic default style for the osmic tile
//! schema (layers named after the 19 `osmic_osm::Layer`s, features
//! classified by their `class` attribute). It is defined once, as data, in
//! this crate.

mod default;
mod error;
mod expr;
mod model;
mod property;
mod value;

pub use default::{
    ATTRIBUTION, DEFAULT_GLYPHS_URL, DEFAULT_SOURCE_ID, StyleOptions, default_style,
    default_style_json, default_style_with,
};
pub use error::{EvalError, StyleError};
pub use expr::{CompareOp, Expr, Interpolation, MAX_EXPRESSION_DEPTH, MatchBranch};
pub use model::{
    BackgroundLayer, CircleLayer, CircleStyle, FillLayer, FillStyle, Layer, LayerKind, LineLayer,
    LineStyle, Style, SymbolLayer, SymbolStyle, VectorSource,
};
pub use property::{
    Alignment, LineCap, LineJoin, Property, PropertyValue, SymbolPlacement, TextAnchor,
    TextTransform,
};
pub use value::{EvalContext, PropertySource, Value, ValueRef};
