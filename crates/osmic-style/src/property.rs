//! Typed layout/paint properties: a constant or an expression.

use osmic_core::Color;
use serde_json::Value as Json;

use crate::error::StyleError;
use crate::expr::{Expr, is_expression};
use crate::value::{EvalContext, Value, number_json};

/// A Rust type a style property can hold.
pub trait PropertyValue: Sized + Clone + PartialEq + std::fmt::Debug {
    /// Name used in error messages.
    const TYPE_NAME: &'static str;

    /// Read the constant (non-expression) JSON form.
    fn from_json(json: &Json) -> Result<Self, String>;

    /// Write the constant JSON form.
    fn to_json(&self) -> Json;

    /// Convert an expression result; `None` falls back to the default.
    fn from_value(value: &Value) -> Option<Self>;

    /// Parse-time validation/coercion of an expression of this type.
    fn prepare(_expr: &mut Expr) -> Result<(), String> {
        Ok(())
    }
}

/// A property value: constant, or an expression evaluated per feature and
/// zoom.
#[derive(Debug, Clone, PartialEq)]
pub enum Property<T> {
    Constant(T),
    Expr(Expr),
}

impl<T: PropertyValue> Property<T> {
    /// Parse a property value from JSON found at `path`.
    pub fn parse(json: &Json, path: &str) -> Result<Self, StyleError> {
        if is_expression(json) {
            let mut expr = Expr::parse_at(json, path)?;
            T::prepare(&mut expr).map_err(|m| StyleError::invalid(path, m))?;
            Ok(Self::Expr(expr))
        } else {
            T::from_json(json)
                .map(Self::Constant)
                .map_err(|m| StyleError::invalid(path, format!("expected {}: {m}", T::TYPE_NAME)))
        }
    }

    /// Serialise to JSON.
    pub fn to_json(&self) -> Json {
        match self {
            Self::Constant(c) => c.to_json(),
            Self::Expr(e) => e.to_json(),
        }
    }

    /// Evaluate; a failing expression yields `default`, as in MapLibre.
    pub fn evaluate(&self, ctx: &EvalContext<'_>, default: &T) -> T {
        match self {
            Self::Constant(c) => c.clone(),
            Self::Expr(e) => e
                .evaluate(ctx)
                .ok()
                .and_then(|v| T::from_value(&v))
                .unwrap_or_else(|| default.clone()),
        }
    }

    /// Whether the value can differ between features.
    pub fn depends_on_feature(&self) -> bool {
        matches!(self, Self::Expr(e) if e.depends_on_feature())
    }

    /// Whether the value can differ between zoom levels.
    pub fn depends_on_zoom(&self) -> bool {
        matches!(self, Self::Expr(e) if e.depends_on_zoom())
    }
}

/// Evaluate an optional property, falling back to `default` when unset.
pub(crate) fn eval_or<T: PropertyValue>(
    p: &Option<Property<T>>,
    ctx: &EvalContext<'_>,
    default: T,
) -> T {
    match p {
        Some(p) => p.evaluate(ctx, &default),
        None => default,
    }
}

impl PropertyValue for f64 {
    const TYPE_NAME: &'static str = "number";

    fn from_json(json: &Json) -> Result<Self, String> {
        json.as_f64().ok_or_else(|| format!("found {json}"))
    }

    fn to_json(&self) -> Json {
        number_json(*self)
    }

    fn from_value(value: &Value) -> Option<Self> {
        match value {
            Value::Number(n) => Some(*n),
            _ => None,
        }
    }
}

impl PropertyValue for bool {
    const TYPE_NAME: &'static str = "boolean";

    fn from_json(json: &Json) -> Result<Self, String> {
        json.as_bool().ok_or_else(|| format!("found {json}"))
    }

    fn to_json(&self) -> Json {
        Json::Bool(*self)
    }

    fn from_value(value: &Value) -> Option<Self> {
        match value {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }
}

impl PropertyValue for String {
    const TYPE_NAME: &'static str = "string";

    fn from_json(json: &Json) -> Result<Self, String> {
        json.as_str()
            .map(str::to_string)
            .ok_or_else(|| format!("found {json}"))
    }

    fn to_json(&self) -> Json {
        Json::String(self.clone())
    }

    fn from_value(value: &Value) -> Option<Self> {
        match value {
            Value::Null | Value::Array(_) => None,
            other => Some(other.stringify()),
        }
    }
}

impl PropertyValue for Color {
    const TYPE_NAME: &'static str = "color";

    fn from_json(json: &Json) -> Result<Self, String> {
        let s = json.as_str().ok_or_else(|| format!("found {json}"))?;
        Color::parse(s).map_err(|e| e.to_string())
    }

    fn to_json(&self) -> Json {
        Json::String(self.to_css())
    }

    fn from_value(value: &Value) -> Option<Self> {
        value.to_color().ok()
    }

    /// Colors appear in expressions as strings; resolve them now so an
    /// invalid color is a parse error rather than a silent default.
    fn prepare(expr: &mut Expr) -> Result<(), String> {
        expr.visit_output_literals(&mut |v| match v {
            Value::String(s) => {
                *v = Value::Color(Color::parse(s).map_err(|e| e.to_string())?);
                Ok(())
            }
            Value::Color(_) => Ok(()),
            other => Err(format!("expected a color, found {}", other.type_name())),
        })
    }
}

impl PropertyValue for Vec<f64> {
    const TYPE_NAME: &'static str = "array of numbers";

    fn from_json(json: &Json) -> Result<Self, String> {
        json.as_array()
            .ok_or_else(|| format!("found {json}"))?
            .iter()
            .map(|v| v.as_f64().ok_or_else(|| format!("found {v}")))
            .collect()
    }

    fn to_json(&self) -> Json {
        Json::Array(self.iter().map(|n| number_json(*n)).collect())
    }

    fn from_value(value: &Value) -> Option<Self> {
        match value {
            Value::Array(items) => items
                .iter()
                .map(|v| match v {
                    Value::Number(n) => Some(*n),
                    _ => None,
                })
                .collect(),
            _ => None,
        }
    }
}

impl PropertyValue for Vec<String> {
    const TYPE_NAME: &'static str = "array of strings";

    fn from_json(json: &Json) -> Result<Self, String> {
        json.as_array()
            .ok_or_else(|| format!("found {json}"))?
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| format!("found {v}"))
            })
            .collect()
    }

    fn to_json(&self) -> Json {
        Json::Array(self.iter().cloned().map(Json::String).collect())
    }

    fn from_value(value: &Value) -> Option<Self> {
        match value {
            Value::Array(items) => items
                .iter()
                .map(|v| match v {
                    Value::String(s) => Some(s.clone()),
                    _ => None,
                })
                .collect(),
            _ => None,
        }
    }
}

macro_rules! string_enum {
    ($(#[$meta:meta])* $name:ident { $($(#[$vmeta:meta])* $variant:ident => $text:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum $name {
            $($(#[$vmeta])* $variant),+
        }

        impl $name {
            /// The style-spec spelling.
            pub const fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $text),+ }
            }

            /// Parse the style-spec spelling.
            pub fn from_name(s: &str) -> Option<Self> {
                match s { $($text => Some(Self::$variant),)+ _ => None }
            }
        }

        impl PropertyValue for $name {
            const TYPE_NAME: &'static str = stringify!($name);

            fn from_json(json: &Json) -> Result<Self, String> {
                let s = json.as_str().ok_or_else(|| format!("found {json}"))?;
                Self::from_name(s).ok_or_else(|| format!("unknown value `{s}`"))
            }

            fn to_json(&self) -> Json {
                Json::String(self.as_str().to_string())
            }

            fn from_value(value: &Value) -> Option<Self> {
                match value {
                    Value::String(s) => Self::from_name(s),
                    _ => None,
                }
            }
        }
    };
}

string_enum! {
    /// `line-cap`.
    LineCap {
        Butt => "butt",
        Round => "round",
        Square => "square",
    }
}

string_enum! {
    /// `line-join`.
    LineJoin {
        Bevel => "bevel",
        Round => "round",
        Miter => "miter",
    }
}

string_enum! {
    /// `symbol-placement`.
    SymbolPlacement {
        /// One label at the feature's anchor point.
        Point => "point",
        /// Labels follow the line geometry.
        Line => "line",
        /// A single label following the middle of the line.
        LineCenter => "line-center",
    }
}

string_enum! {
    /// `text-anchor`: which part of the text sits on the anchor point.
    TextAnchor {
        Center => "center",
        Left => "left",
        Right => "right",
        Top => "top",
        Bottom => "bottom",
        TopLeft => "top-left",
        TopRight => "top-right",
        BottomLeft => "bottom-left",
        BottomRight => "bottom-right",
    }
}

impl TextAnchor {
    /// The point of the text box (`[0,0]` top-left, `[1,1]` bottom-right)
    /// that is placed on the anchor.
    pub const fn fraction(self) -> [f32; 2] {
        match self {
            Self::Center => [0.5, 0.5],
            Self::Left => [0.0, 0.5],
            Self::Right => [1.0, 0.5],
            Self::Top => [0.5, 0.0],
            Self::Bottom => [0.5, 1.0],
            Self::TopLeft => [0.0, 0.0],
            Self::TopRight => [1.0, 0.0],
            Self::BottomLeft => [0.0, 1.0],
            Self::BottomRight => [1.0, 1.0],
        }
    }
}

string_enum! {
    /// `text-transform`.
    TextTransform {
        None => "none",
        Uppercase => "uppercase",
        Lowercase => "lowercase",
    }
}

impl TextTransform {
    /// Apply the transform to `text`.
    pub fn apply(self, text: &str) -> String {
        match self {
            Self::None => text.to_string(),
            Self::Uppercase => text.to_uppercase(),
            Self::Lowercase => text.to_lowercase(),
        }
    }
}

string_enum! {
    /// `text-rotation-alignment`.
    Alignment {
        Map => "map",
        Viewport => "viewport",
        Auto => "auto",
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn constant_and_expression_forms() {
        let p = Property::<f64>::parse(&json!(3), "p").unwrap();
        assert_eq!(p, Property::Constant(3.0));
        let p = Property::<f64>::parse(&json!(["zoom"]), "p").unwrap();
        assert!(matches!(p, Property::Expr(_)));
        assert!(Property::<f64>::parse(&json!("x"), "p").is_err());
    }

    #[test]
    fn invalid_color_is_a_parse_error() {
        let err = Property::<Color>::parse(&json!("not-a-color"), "paint.fill-color").unwrap_err();
        assert!(err.to_string().contains("paint.fill-color"), "{err}");
        let err = Property::<Color>::parse(
            &json!(["match", ["get", "c"], "a", "#fff", "bogus"]),
            "paint.fill-color",
        )
        .unwrap_err();
        assert!(err.to_string().contains("bogus"), "{err}");
    }

    #[test]
    fn expression_colors_are_resolved_at_parse_time() {
        let p = Property::<Color>::parse(
            &json!([
                "match",
                ["get", "c"],
                "a",
                "rgb(255,0,0)",
                "hsl(120,100%,50%)"
            ]),
            "p",
        )
        .unwrap();
        let ctx_props = [("c", "a")];
        let ctx = EvalContext::new(0.0, &ctx_props);
        assert_eq!(p.evaluate(&ctx, &Color::BLACK).to_rgba8(), [255, 0, 0, 255]);
        let ctx_props = [("c", "zzz")];
        let ctx = EvalContext::new(0.0, &ctx_props);
        assert_eq!(p.evaluate(&ctx, &Color::BLACK).to_rgba8(), [0, 255, 0, 255]);
    }

    #[test]
    fn failing_expression_uses_default() {
        let p = Property::<f64>::parse(&json!(["to-number", ["get", "x"]]), "p").unwrap();
        assert_eq!(p.evaluate(&EvalContext::at_zoom(0.0), &2.5), 0.0);
        let p = Property::<f64>::parse(&json!(["<", ["get", "x"], 1]), "p").unwrap();
        // A boolean result is not a number: default.
        assert_eq!(p.evaluate(&EvalContext::at_zoom(0.0), &2.5), 2.5);
    }

    #[test]
    fn arrays_distinguish_constants_from_expressions() {
        let p = Property::<Vec<String>>::parse(&json!(["Open Sans Regular"]), "p").unwrap();
        assert_eq!(p, Property::Constant(vec!["Open Sans Regular".to_string()]));
        let p = Property::<Vec<f64>>::parse(&json!([4, 2]), "p").unwrap();
        assert_eq!(p, Property::Constant(vec![4.0, 2.0]));
    }

    #[test]
    fn enums_round_trip() {
        for a in [LineCap::Butt, LineCap::Round, LineCap::Square] {
            assert_eq!(LineCap::from_name(a.as_str()), Some(a));
        }
        assert!(Property::<LineCap>::parse(&json!("flat"), "p").is_err());
    }
}
