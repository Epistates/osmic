//! Runtime values produced by expressions.

use std::collections::{BTreeMap, HashMap};

use osmic_core::Color;
use serde_json::Value as Json;

use crate::error::EvalError;

/// A value an [`crate::Expr`] evaluates to.
///
/// Feature attributes keep their type: a vector-tile number is a
/// [`Value::Number`] and a boolean a [`Value::Bool`], so filters such as
/// `["==", ["get", "admin_level"], 2]` compare like MapLibre's.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Value {
    /// JSON `null`; also what `get` yields for a missing attribute.
    Null,
    /// A boolean.
    Bool(bool),
    /// A number (MapLibre numbers are IEEE doubles).
    Number(f64),
    /// A string.
    String(String),
    /// A color: a color-typed property's literal, or an interpolation
    /// result.
    Color(Color),
    /// An array (from `["literal", [...]]`).
    Array(Vec<Value>),
}

/// A borrowed view of a [`Value`]: what [`PropertySource::property`]
/// returns, so that reading an attribute never allocates.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum ValueRef<'a> {
    /// JSON `null`.
    Null,
    /// A boolean.
    Bool(bool),
    /// A number.
    Number(f64),
    /// A string.
    String(&'a str),
    /// A color.
    Color(Color),
    /// An array.
    Array(&'a [Value]),
}

impl Value {
    /// The JSON-ish type name used in error messages.
    pub fn type_name(&self) -> &'static str {
        self.view().type_name()
    }

    /// A borrowed view of this value.
    pub fn view(&self) -> ValueRef<'_> {
        ValueRef::from(self)
    }

    /// Read a JSON literal found `depth` levels deep in an expression;
    /// nested arrays count towards [`crate::MAX_EXPRESSION_DEPTH`].
    pub(crate) fn from_json(json: &Json, depth: usize) -> Result<Self, String> {
        if depth > crate::expr::MAX_EXPRESSION_DEPTH {
            return Err(crate::expr::depth_message());
        }
        Ok(match json {
            Json::Null => Self::Null,
            Json::Bool(b) => Self::Bool(*b),
            Json::Number(n) => Self::Number(n.as_f64().ok_or("number out of range")?),
            Json::String(s) => Self::String(s.clone()),
            Json::Array(a) => Self::Array(
                a.iter()
                    .map(|v| Self::from_json(v, depth + 1))
                    .collect::<Result<_, _>>()?,
            ),
            Json::Object(_) => return Err("object literals are not supported".into()),
        })
    }

    /// Serialise a literal. Arrays are returned bare; the expression layer
    /// wraps them in `["literal", ...]`.
    pub(crate) fn to_json(&self) -> Json {
        match self {
            Self::Null => Json::Null,
            Self::Bool(b) => Json::Bool(*b),
            Self::Number(n) => number_json(*n),
            Self::String(s) => Json::String(s.clone()),
            Self::Color(c) => Json::String(c.to_css()),
            Self::Array(a) => Json::Array(a.iter().map(Self::to_json).collect()),
        }
    }

    /// Coerce to a color: colors pass through, strings are parsed as CSS.
    pub fn to_color(&self) -> Result<Color, EvalError> {
        self.view().to_color()
    }
}

impl<'a> From<&'a Value> for ValueRef<'a> {
    fn from(value: &'a Value) -> Self {
        match value {
            Value::Null => Self::Null,
            Value::Bool(b) => Self::Bool(*b),
            Value::Number(n) => Self::Number(*n),
            Value::String(s) => Self::String(s),
            Value::Color(c) => Self::Color(*c),
            Value::Array(a) => Self::Array(a),
        }
    }
}

impl ValueRef<'_> {
    /// The JSON-ish type name used in error messages.
    pub fn type_name(self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::Bool(_) => "boolean",
            Self::Number(_) => "number",
            Self::String(_) => "string",
            Self::Color(_) => "color",
            Self::Array(_) => "array",
        }
    }

    /// An owned copy.
    pub fn to_value(self) -> Value {
        match self {
            Self::Null => Value::Null,
            Self::Bool(b) => Value::Bool(b),
            Self::Number(n) => Value::Number(n),
            Self::String(s) => Value::String(s.to_string()),
            Self::Color(c) => Value::Color(c),
            Self::Array(a) => Value::Array(a.to_vec()),
        }
    }

    /// Coerce to a color: colors pass through, strings are parsed as CSS.
    pub fn to_color(self) -> Result<Color, EvalError> {
        match self {
            Self::Color(c) => Ok(c),
            Self::String(s) => Color::parse(s).map_err(|e| EvalError::Color(e.to_string())),
            other => Err(type_error("color", "color", other)),
        }
    }

    pub(crate) fn expect_number(self, op: &'static str) -> Result<f64, EvalError> {
        match self {
            Self::Number(n) => Ok(n),
            other => Err(type_error(op, "number", other)),
        }
    }

    pub(crate) fn expect_bool(self, op: &'static str) -> Result<bool, EvalError> {
        match self {
            Self::Bool(b) => Ok(b),
            other => Err(type_error(op, "boolean", other)),
        }
    }

    /// MapLibre `to-string`.
    pub(crate) fn stringify(self) -> String {
        match self {
            Self::Null => String::new(),
            Self::Bool(b) => b.to_string(),
            Self::Number(n) => n.to_string(),
            Self::String(s) => s.to_string(),
            Self::Color(c) => c.to_css(),
            Self::Array(a) => {
                let items: Vec<String> = a.iter().map(|v| v.view().json_text()).collect();
                format!("[{}]", items.join(","))
            }
        }
    }

    fn json_text(self) -> String {
        match self {
            Self::String(s) => format!("{s:?}"),
            other => other.stringify(),
        }
    }
}

pub(crate) fn type_error(
    op: &'static str,
    expected: &'static str,
    found: ValueRef<'_>,
) -> EvalError {
    EvalError::Type {
        op,
        expected,
        found: found.type_name(),
    }
}

/// A JSON number, integral when the value is.
pub(crate) fn number_json(n: f64) -> Json {
    if n.is_finite() && n.fract() == 0.0 && n.abs() < 9.0e15 {
        Json::from(n as i64)
    } else {
        serde_json::Number::from_f64(n).map_or(Json::Null, Json::Number)
    }
}

/// Where `get`/`has` read feature attributes from.
pub trait PropertySource {
    /// The attribute `key`, or `None` if the feature lacks it.
    fn property(&self, key: &str) -> Option<ValueRef<'_>>;
}

impl PropertySource for HashMap<String, Value> {
    fn property(&self, key: &str) -> Option<ValueRef<'_>> {
        self.get(key).map(ValueRef::from)
    }
}

impl PropertySource for BTreeMap<String, Value> {
    fn property(&self, key: &str) -> Option<ValueRef<'_>> {
        self.get(key).map(ValueRef::from)
    }
}

/// String attribute pairs, e.g. `[("class", "primary")]`.
impl<K: AsRef<str>, V: AsRef<str>, const N: usize> PropertySource for [(K, V); N] {
    fn property(&self, key: &str) -> Option<ValueRef<'_>> {
        pairs_property(self, key)
    }
}

/// String attribute pairs.
impl<K: AsRef<str>, V: AsRef<str>> PropertySource for Vec<(K, V)> {
    fn property(&self, key: &str) -> Option<ValueRef<'_>> {
        pairs_property(self, key)
    }
}

fn pairs_property<'a, K: AsRef<str>, V: AsRef<str>>(
    pairs: &'a [(K, V)],
    key: &str,
) -> Option<ValueRef<'a>> {
    pairs
        .iter()
        .find(|(k, _)| k.as_ref() == key)
        .map(|(_, v)| ValueRef::String(v.as_ref()))
}

/// Everything an expression can observe.
#[derive(Clone, Copy)]
pub struct EvalContext<'a> {
    /// Map zoom (MapLibre convention: 512-px tiles).
    pub zoom: f64,
    /// The feature's attributes; `None` for feature-independent evaluation
    /// (`get` then yields null).
    pub feature: Option<&'a dyn PropertySource>,
}

impl<'a> EvalContext<'a> {
    /// A context without a feature.
    pub fn at_zoom(zoom: f64) -> Self {
        Self {
            zoom,
            feature: None,
        }
    }

    /// A context for `feature` at `zoom`.
    pub fn new(zoom: f64, feature: &'a dyn PropertySource) -> Self {
        Self {
            zoom,
            feature: Some(feature),
        }
    }
}
