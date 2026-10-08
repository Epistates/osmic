//! Runtime values produced by expressions.

use std::collections::{BTreeMap, HashMap};

use osmic_core::Color;
use serde_json::Value as Json;

use crate::error::EvalError;

/// A value an [`crate::Expr`] evaluates to.
///
/// Vector-tile attributes are decoded as strings, so `get` yields
/// [`Value::String`]; wrap numeric attributes in `to-number` before
/// comparing them with numbers.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Color(Color),
    Array(Vec<Value>),
}

impl Value {
    /// The JSON-ish type name used in error messages.
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::Bool(_) => "boolean",
            Self::Number(_) => "number",
            Self::String(_) => "string",
            Self::Color(_) => "color",
            Self::Array(_) => "array",
        }
    }

    pub(crate) fn from_json(json: &Json) -> Result<Self, String> {
        Ok(match json {
            Json::Null => Self::Null,
            Json::Bool(b) => Self::Bool(*b),
            Json::Number(n) => Self::Number(n.as_f64().ok_or("number out of range")?),
            Json::String(s) => Self::String(s.clone()),
            Json::Array(a) => Self::Array(a.iter().map(Self::from_json).collect::<Result<_, _>>()?),
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
        match self {
            Self::Color(c) => Ok(*c),
            Self::String(s) => Color::parse(s).map_err(|e| EvalError::new(e.to_string())),
            other => Err(EvalError::new(format!(
                "expected color, found {}",
                other.type_name()
            ))),
        }
    }

    pub(crate) fn expect_number(&self) -> Result<f64, EvalError> {
        match self {
            Self::Number(n) => Ok(*n),
            other => Err(EvalError::new(format!(
                "expected number, found {}",
                other.type_name()
            ))),
        }
    }

    pub(crate) fn expect_bool(&self) -> Result<bool, EvalError> {
        match self {
            Self::Bool(b) => Ok(*b),
            other => Err(EvalError::new(format!(
                "expected boolean, found {}",
                other.type_name()
            ))),
        }
    }

    /// MapLibre `to-string`.
    pub(crate) fn stringify(&self) -> String {
        match self {
            Self::Null => String::new(),
            Self::Bool(b) => b.to_string(),
            Self::Number(n) => n.to_string(),
            Self::String(s) => s.clone(),
            Self::Color(c) => c.to_css(),
            Self::Array(a) => {
                let items: Vec<String> = a.iter().map(Self::json_text).collect();
                format!("[{}]", items.join(","))
            }
        }
    }

    fn json_text(&self) -> String {
        match self {
            Self::String(s) => format!("{s:?}"),
            other => other.stringify(),
        }
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
    fn property(&self, key: &str) -> Option<Value>;
}

impl PropertySource for HashMap<String, Value> {
    fn property(&self, key: &str) -> Option<Value> {
        self.get(key).cloned()
    }
}

impl PropertySource for BTreeMap<String, Value> {
    fn property(&self, key: &str) -> Option<Value> {
        self.get(key).cloned()
    }
}

/// String attribute pairs, e.g. `[("class", "primary")]`.
impl<K: AsRef<str>, V: AsRef<str>, const N: usize> PropertySource for [(K, V); N] {
    fn property(&self, key: &str) -> Option<Value> {
        pairs_property(self, key)
    }
}

/// String attribute pairs.
impl<K: AsRef<str>, V: AsRef<str>> PropertySource for Vec<(K, V)> {
    fn property(&self, key: &str) -> Option<Value> {
        pairs_property(self, key)
    }
}

fn pairs_property<K: AsRef<str>, V: AsRef<str>>(pairs: &[(K, V)], key: &str) -> Option<Value> {
    pairs
        .iter()
        .find(|(k, _)| k.as_ref() == key)
        .map(|(_, v)| Value::String(v.as_ref().to_string()))
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
