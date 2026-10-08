//! The expression language: parse, evaluate, serialise.
//!
//! Supported operators: literals (including `["literal", ...]`), `get`,
//! `has`, `!has`, `==`, `!=`, `<`, `<=`, `>`, `>=`, `!`, `all`, `any`,
//! `in`, `!in`, `match`, `case`, `coalesce`, `zoom`, `interpolate`
//! (`linear`, `exponential`), `step`, `to-string` and `to-number`.
//!
//! Layer filters additionally accept the legacy syntax (`["==", "class",
//! "x"]`, `["in", "class", "a", "b"]`, `["!has", "name"]`, `none`, ...),
//! which is normalised to the same [`Expr`] tree at parse time. Any other
//! operator is rejected with [`StyleError::Unsupported`] naming it.

use osmic_core::Color;
use serde_json::Value as Json;

use crate::error::{EvalError, StyleError};
use crate::value::{EvalContext, Value, ValueRef, number_json, type_error};

/// A comparison operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareOp {
    /// `==`: equal values of the same type.
    Eq,
    /// `!=`: not `==`.
    Ne,
    /// `<` (numbers or strings).
    Lt,
    /// `<=` (numbers or strings).
    Le,
    /// `>` (numbers or strings).
    Gt,
    /// `>=` (numbers or strings).
    Ge,
}

impl CompareOp {
    fn name(self) -> &'static str {
        match self {
            Self::Eq => "==",
            Self::Ne => "!=",
            Self::Lt => "<",
            Self::Le => "<=",
            Self::Gt => ">",
            Self::Ge => ">=",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "==" => Self::Eq,
            "!=" => Self::Ne,
            "<" => Self::Lt,
            "<=" => Self::Le,
            ">" => Self::Gt,
            ">=" => Self::Ge,
            _ => return None,
        })
    }
}

/// How `interpolate` blends between stops.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Interpolation {
    /// `["linear"]`.
    Linear,
    /// Exponential easing with the given base (`1` is linear).
    Exponential(f64),
}

/// One `match` arm: the input equals any of `labels`.
#[derive(Debug, Clone, PartialEq)]
pub struct MatchBranch {
    /// The values this arm matches: all strings or all numbers.
    pub labels: Vec<Value>,
    /// The result when it matches.
    pub output: Expr,
}

/// A parsed expression.
///
/// Built by [`Expr::parse`] / [`Expr::parse_filter`] (or the helper
/// constructors), evaluated with [`Expr::evaluate`] and written back with
/// [`Expr::to_json`].
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Expr {
    /// A constant (including `["literal", ...]` arrays).
    Literal(Value),
    /// `["zoom"]`.
    Zoom,
    /// `["get", key]`: the feature attribute, or null.
    Get(Box<Expr>),
    /// `["has", key]`.
    Has(Box<Expr>),
    /// `["!", x]`.
    Not(Box<Expr>),
    /// `[op, a, b]` for the comparison operators.
    Compare(CompareOp, Box<Expr>, Box<Expr>),
    /// `["all", ...]`: true when every operand is (short-circuits).
    All(Vec<Expr>),
    /// `["any", ...]`: true when some operand is (short-circuits).
    Any(Vec<Expr>),
    /// `["in", needle, haystack]`: array membership or substring.
    In(Box<Expr>, Box<Expr>),
    /// `["match", input, labels, output, ..., fallback]`.
    Match {
        /// The value compared with the labels.
        input: Box<Expr>,
        /// The arms, tried in order.
        branches: Vec<MatchBranch>,
        /// The result when no arm matches.
        fallback: Box<Expr>,
    },
    /// `["case", condition, output, ..., fallback]`.
    Case {
        /// `(condition, output)` pairs, tried in order.
        branches: Vec<(Expr, Expr)>,
        /// The result when no condition holds.
        fallback: Box<Expr>,
    },
    /// `["coalesce", ...]`: the first non-null operand.
    Coalesce(Vec<Expr>),
    /// `["interpolate", interpolation, input, stop, output, ...]`.
    Interpolate {
        /// How to blend between stops.
        interpolation: Interpolation,
        /// The numeric input (usually `["zoom"]`).
        input: Box<Expr>,
        /// `(stop input, output)` pairs, strictly ascending.
        stops: Vec<(f64, Expr)>,
    },
    /// `["step", input, base, stop, output, ...]`.
    Step {
        /// The numeric input.
        input: Box<Expr>,
        /// The result below the first stop.
        base: Box<Expr>,
        /// `(stop input, output)` pairs, strictly ascending.
        stops: Vec<(f64, Expr)>,
    },
    /// `["to-string", x]`.
    ToString(Box<Expr>),
    /// `["to-number", x, ...]`: the first operand convertible to a number.
    ToNumber(Vec<Expr>),
}

/// Deepest nesting of expression arrays (including nested literal arrays)
/// the parser accepts. Deeper documents are rejected with
/// [`StyleError::Invalid`] so that parsing, evaluation and dropping an
/// expression can never exhaust the stack.
pub const MAX_EXPRESSION_DEPTH: usize = 256;

pub(crate) fn depth_message() -> String {
    format!("expression nested deeper than {MAX_EXPRESSION_DEPTH} levels")
}

fn check_depth(depth: usize, path: &str) -> Result<(), StyleError> {
    if depth > MAX_EXPRESSION_DEPTH {
        Err(StyleError::invalid(path, depth_message()))
    } else {
        Ok(())
    }
}

/// Operators [`Expr::parse`] accepts. (`none` and the legacy forms of the
/// others are filter-only; see [`Expr::parse_filter`].)
const SUPPORTED_OPS: &[&str] = &[
    "literal",
    "get",
    "has",
    "!has",
    "==",
    "!=",
    "<",
    "<=",
    ">",
    ">=",
    "!",
    "all",
    "any",
    "in",
    "!in",
    "match",
    "case",
    "coalesce",
    "zoom",
    "interpolate",
    "step",
    "to-string",
    "to-number",
];

/// MapLibre operators outside the supported subset. Used to tell "valid
/// MapLibre, not implemented here" from a typo, and to recognise
/// expressions in constant-vs-expression disambiguation.
const UNSUPPORTED_OPS: &[&str] = &[
    "array",
    "boolean",
    "collator",
    "format",
    "image",
    "number",
    "number-format",
    "object",
    "string",
    "to-boolean",
    "to-color",
    "typeof",
    "feature-state",
    "geometry-type",
    "id",
    "line-progress",
    "properties",
    "accumulated",
    "heatmap-density",
    "elevation",
    "let",
    "var",
    "concat",
    "downcase",
    "upcase",
    "is-supported-script",
    "resolved-locale",
    "rgb",
    "rgba",
    "to-rgba",
    "e",
    "ln2",
    "pi",
    "ln",
    "log10",
    "log2",
    "sin",
    "cos",
    "tan",
    "asin",
    "acos",
    "atan",
    "min",
    "max",
    "round",
    "abs",
    "ceil",
    "floor",
    "distance",
    "sqrt",
    "+",
    "-",
    "*",
    "/",
    "%",
    "^",
    "within",
    "at",
    "index-of",
    "length",
    "slice",
    "interpolate-hcl",
    "interpolate-lab",
];

/// Whether `json` is an expression (an array headed by a known operator)
/// rather than a constant array.
pub(crate) fn is_expression(json: &Json) -> bool {
    match json {
        Json::Array(items) => match items.first() {
            Some(Json::String(op)) => {
                SUPPORTED_OPS.contains(&op.as_str()) || UNSUPPORTED_OPS.contains(&op.as_str())
            }
            _ => false,
        },
        _ => false,
    }
}

fn child_path(path: &str, index: usize) -> String {
    format!("{path}[{index}]")
}

impl Expr {
    /// A string literal.
    pub fn string(s: impl Into<String>) -> Self {
        Self::Literal(Value::String(s.into()))
    }

    /// A number literal.
    pub fn number(n: f64) -> Self {
        Self::Literal(Value::Number(n))
    }

    /// `["get", key]`.
    pub fn get(key: &str) -> Self {
        Self::Get(Box::new(Self::string(key)))
    }

    /// `["has", key]`.
    pub fn has(key: &str) -> Self {
        Self::Has(Box::new(Self::string(key)))
    }

    /// `["match", ["get", key], ...]` with string labels; each entry maps a
    /// set of values to one output.
    pub fn match_get(key: &str, arms: Vec<(Vec<&str>, Expr)>, fallback: Expr) -> Self {
        Self::Match {
            input: Box::new(Self::get(key)),
            branches: arms
                .into_iter()
                .map(|(labels, output)| MatchBranch {
                    labels: labels
                        .into_iter()
                        .map(|l| Value::String(l.into()))
                        .collect(),
                    output,
                })
                .collect(),
            fallback: Box::new(fallback),
        }
    }

    /// `["match", ["get", key], [values...], true, false]`: true when the
    /// attribute is one of `values`.
    pub fn get_in(key: &str, values: &[&str]) -> Self {
        Self::match_get(
            key,
            vec![(values.to_vec(), Self::Literal(Value::Bool(true)))],
            Self::Literal(Value::Bool(false)),
        )
    }

    /// Linear/exponential interpolation over the map zoom.
    pub fn interpolate_zoom(interpolation: Interpolation, stops: Vec<(f64, Expr)>) -> Self {
        Self::Interpolate {
            interpolation,
            input: Box::new(Self::Zoom),
            stops,
        }
    }

    /// Parse an expression (no legacy syntax).
    pub fn parse(json: &Json) -> Result<Self, StyleError> {
        Self::parse_at(json, "expression")
    }

    /// Parse an expression, reporting errors under `path`.
    ///
    /// Expressions nested deeper than [`MAX_EXPRESSION_DEPTH`] are
    /// rejected.
    pub fn parse_at(json: &Json, path: &str) -> Result<Self, StyleError> {
        parse_expr(json, path, 0)
    }

    /// Parse a layer filter: expression syntax or the legacy syntax.
    pub fn parse_filter(json: &Json, path: &str) -> Result<Self, StyleError> {
        parse_filter_at(json, path, 0)
    }
}

fn parse_expr(json: &Json, path: &str, depth: usize) -> Result<Expr, StyleError> {
    check_depth(depth, path)?;
    match json {
        Json::Array(items) => parse_call(items, path, depth),
        Json::Object(_) => Err(StyleError::unsupported(
            path,
            "expression",
            "object literal",
        )),
        scalar => Value::from_json(scalar, depth)
            .map(Expr::Literal)
            .map_err(|m| StyleError::invalid(path, m)),
    }
}

fn parse_filter_at(json: &Json, path: &str, depth: usize) -> Result<Expr, StyleError> {
    check_depth(depth, path)?;
    let Json::Array(items) = json else {
        return parse_expr(json, path, depth);
    };
    let Some(Json::String(op)) = items.first() else {
        return parse_expr(json, path, depth);
    };
    let args = &items[1..];
    let legacy_key = |i: usize| -> Result<String, StyleError> {
        let p = child_path(path, i + 1);
        match args.get(i) {
            Some(Json::String(k)) if k.starts_with('$') => {
                Err(StyleError::unsupported(&p, "legacy filter key", k))
            }
            Some(Json::String(k)) => Ok(k.clone()),
            _ => Err(StyleError::invalid(&p, "expected a property name")),
        }
    };
    let children = || {
        args.iter()
            .enumerate()
            .map(|(i, a)| parse_filter_at(a, &child_path(path, i + 1), depth + 1))
            .collect::<Result<Vec<_>, _>>()
    };
    match op.as_str() {
        "all" => Ok(Expr::All(children()?)),
        "any" => Ok(Expr::Any(children()?)),
        "none" => Ok(Expr::Not(Box::new(Expr::Any(children()?)))),
        "has" | "!has" if matches!(args, [Json::String(_)]) => {
            let has = Expr::has(&legacy_key(0)?);
            Ok(if op == "has" {
                has
            } else {
                Expr::Not(Box::new(has))
            })
        }
        "in" | "!in" => {
            // MapLibre's disambiguation: a string key followed by a
            // non-array second operand is the legacy form.
            let is_expression = op == "in"
                && args.len() >= 2
                && (!matches!(args[0], Json::String(_)) || matches!(args[1], Json::Array(_)));
            if is_expression {
                return parse_expr(json, path, depth);
            }
            let key = legacy_key(0)?;
            let mut labels: Vec<Value> = Vec::with_capacity(args.len().saturating_sub(1));
            for (i, v) in args.iter().enumerate().skip(1) {
                let v = legacy_value(v, &child_path(path, i + 1))?;
                if !labels.contains(&v) {
                    labels.push(v);
                }
            }
            let test = legacy_membership(&key, labels);
            Ok(if op == "in" {
                test
            } else {
                Expr::Not(Box::new(test))
            })
        }
        name if CompareOp::from_name(name).is_some() => {
            let cmp = CompareOp::from_name(name).expect("checked");
            let is_expression = args.len() == 2
                && matches!(
                    (&args[0], &args[1]),
                    (Json::Array(_), _) | (_, Json::Array(_))
                );
            if is_expression || args.len() != 2 {
                return parse_expr(json, path, depth);
            }
            let key = Expr::get(&legacy_key(0)?);
            let value = legacy_value(&args[1], &child_path(path, 2))?;
            Ok(Expr::Compare(
                cmp,
                Box::new(key),
                Box::new(Expr::Literal(value)),
            ))
        }
        _ => parse_expr(json, path, depth),
    }
}

/// The expression for a legacy `["in", key, labels...]` filter (labels
/// already deduplicated).
///
/// MapLibre's `match` only takes unique labels that are all strings or all
/// numbers, so other label sets become `["in", ["get", key], ["literal",
/// [...]]]`. Either form serialises to valid MapLibre JSON.
fn legacy_membership(key: &str, labels: Vec<Value>) -> Expr {
    let homogeneous = labels.iter().all(|l| matches!(l, Value::String(_)))
        || labels.iter().all(|l| matches!(l, Value::Number(_)));
    if labels.is_empty() {
        Expr::Literal(Value::Bool(false))
    } else if homogeneous {
        Expr::Match {
            input: Box::new(Expr::get(key)),
            branches: vec![MatchBranch {
                labels,
                output: Expr::Literal(Value::Bool(true)),
            }],
            fallback: Box::new(Expr::Literal(Value::Bool(false))),
        }
    } else {
        Expr::In(
            Box::new(Expr::get(key)),
            Box::new(Expr::Literal(Value::Array(labels))),
        )
    }
}

impl Expr {
    /// Serialise to MapLibre expression JSON.
    pub fn to_json(&self) -> Json {
        fn call(op: &str, args: impl IntoIterator<Item = Json>) -> Json {
            let mut v = vec![Json::String(op.to_string())];
            v.extend(args);
            Json::Array(v)
        }
        match self {
            Self::Literal(Value::Array(a)) => call(
                "literal",
                [Json::Array(a.iter().map(Value::to_json).collect())],
            ),
            Self::Literal(v) => v.to_json(),
            Self::Zoom => call("zoom", []),
            Self::Get(k) => call("get", [k.to_json()]),
            Self::Has(k) => call("has", [k.to_json()]),
            Self::Not(x) => call("!", [x.to_json()]),
            Self::Compare(op, a, b) => call(op.name(), [a.to_json(), b.to_json()]),
            Self::All(xs) => call("all", xs.iter().map(Self::to_json)),
            Self::Any(xs) => call("any", xs.iter().map(Self::to_json)),
            Self::In(a, b) => call("in", [a.to_json(), b.to_json()]),
            Self::Match {
                input,
                branches,
                fallback,
            } => {
                let mut args = vec![input.to_json()];
                for b in branches {
                    args.push(match b.labels.as_slice() {
                        [one] => one.to_json(),
                        many => Json::Array(many.iter().map(Value::to_json).collect()),
                    });
                    args.push(b.output.to_json());
                }
                args.push(fallback.to_json());
                call("match", args)
            }
            Self::Case { branches, fallback } => {
                let mut args = Vec::new();
                for (cond, out) in branches {
                    args.push(cond.to_json());
                    args.push(out.to_json());
                }
                args.push(fallback.to_json());
                call("case", args)
            }
            Self::Coalesce(xs) => call("coalesce", xs.iter().map(Self::to_json)),
            Self::Interpolate {
                interpolation,
                input,
                stops,
            } => {
                let kind = match interpolation {
                    Interpolation::Linear => call("linear", []),
                    Interpolation::Exponential(base) => call("exponential", [number_json(*base)]),
                };
                let mut args = vec![kind, input.to_json()];
                for (stop, out) in stops {
                    args.push(number_json(*stop));
                    args.push(out.to_json());
                }
                call("interpolate", args)
            }
            Self::Step { input, base, stops } => {
                let mut args = vec![input.to_json(), base.to_json()];
                for (stop, out) in stops {
                    args.push(number_json(*stop));
                    args.push(out.to_json());
                }
                call("step", args)
            }
            Self::ToString(x) => call("to-string", [x.to_json()]),
            Self::ToNumber(xs) => call("to-number", xs.iter().map(Self::to_json)),
        }
    }

    /// Evaluate to a [`Value`].
    pub fn evaluate(&self, ctx: &EvalContext<'_>) -> Result<Value, EvalError> {
        self.eval(ctx).map(Eval::into_value)
    }

    /// Evaluate as a filter: only a boolean `true` matches; errors and
    /// other values do not.
    pub fn evaluate_bool(&self, ctx: &EvalContext<'_>) -> bool {
        matches!(
            self.eval(ctx).as_ref().map(Eval::view),
            Ok(ValueRef::Bool(true))
        )
    }

    /// Evaluate, borrowing literals and feature attributes rather than
    /// copying them: only `to-string` produces an owned result.
    pub(crate) fn eval<'a>(&'a self, ctx: &EvalContext<'a>) -> Result<Eval<'a>, EvalError> {
        let bool_of = |x: &Expr, op| x.eval(ctx)?.view().expect_bool(op);
        Ok(Eval::Ref(match self {
            Self::Literal(v) => v.view(),
            Self::Zoom => ValueRef::Number(ctx.zoom),
            Self::Get(key) => {
                let key = key.eval(ctx)?;
                let key = expect_string("get", key.view())?;
                ctx.feature
                    .and_then(|f| f.property(key))
                    .unwrap_or(ValueRef::Null)
            }
            Self::Has(key) => {
                let key = key.eval(ctx)?;
                let key = expect_string("has", key.view())?;
                ValueRef::Bool(ctx.feature.is_some_and(|f| f.property(key).is_some()))
            }
            Self::Not(x) => ValueRef::Bool(!bool_of(x, "!")?),
            Self::Compare(op, a, b) => {
                ValueRef::Bool(compare(*op, a.eval(ctx)?.view(), b.eval(ctx)?.view())?)
            }
            Self::All(xs) => {
                for x in xs {
                    if !bool_of(x, "all")? {
                        return Ok(Eval::Ref(ValueRef::Bool(false)));
                    }
                }
                ValueRef::Bool(true)
            }
            Self::Any(xs) => {
                for x in xs {
                    if bool_of(x, "any")? {
                        return Ok(Eval::Ref(ValueRef::Bool(true)));
                    }
                }
                ValueRef::Bool(false)
            }
            Self::In(needle, haystack) => {
                let (needle, haystack) = (needle.eval(ctx)?, haystack.eval(ctx)?);
                let needle = needle.view();
                ValueRef::Bool(match haystack.view() {
                    ValueRef::Array(items) => items.iter().any(|i| i.view() == needle),
                    ValueRef::String(h) => h.contains(expect_string("in", needle)?),
                    other => return Err(type_error("in", "array or string", other)),
                })
            }
            Self::Match {
                input,
                branches,
                fallback,
            } => {
                let input = input.eval(ctx)?;
                let input = input.view();
                let arm = branches
                    .iter()
                    .find(|b| b.labels.iter().any(|l| l.view() == input));
                return match arm {
                    Some(b) => b.output.eval(ctx),
                    None => fallback.eval(ctx),
                };
            }
            Self::Case { branches, fallback } => {
                for (cond, out) in branches {
                    if bool_of(cond, "case")? {
                        return out.eval(ctx);
                    }
                }
                return fallback.eval(ctx);
            }
            // As in MapLibre, an operand that fails to evaluate fails the
            // whole expression; only null results fall through.
            Self::Coalesce(xs) => {
                for x in xs {
                    let v = x.eval(ctx)?;
                    if v.view() != ValueRef::Null {
                        return Ok(v);
                    }
                }
                ValueRef::Null
            }
            Self::Interpolate {
                interpolation,
                input,
                stops,
            } => {
                let x = finite_input("interpolate", input.eval(ctx)?.view())?;
                return interpolate(*interpolation, x, stops, ctx);
            }
            Self::Step { input, base, stops } => {
                let x = finite_input("step", input.eval(ctx)?.view())?;
                return match stops.iter().rev().find(|(stop, _)| *stop <= x) {
                    Some((_, out)) => out.eval(ctx),
                    None => base.eval(ctx),
                };
            }
            Self::ToString(x) => return Ok(Eval::String(x.eval(ctx)?.view().stringify())),
            Self::ToNumber(xs) => {
                let mut found = "null";
                for x in xs {
                    let v = x.eval(ctx)?;
                    if let Some(n) = to_number(v.view()) {
                        return Ok(Eval::Ref(ValueRef::Number(n)));
                    }
                    found = v.view().type_name();
                }
                return Err(EvalError::NotANumber { found });
            }
        }))
    }

    /// Whether the result can vary between features (reads attributes).
    pub fn depends_on_feature(&self) -> bool {
        self.any_node(&|e| matches!(e, Self::Get(_) | Self::Has(_)))
    }

    /// Whether the result can vary with the zoom level.
    pub fn depends_on_zoom(&self) -> bool {
        self.any_node(&|e| matches!(e, Self::Zoom))
    }

    fn children(&self) -> Vec<&Expr> {
        match self {
            Self::Literal(_) | Self::Zoom => vec![],
            Self::Get(a) | Self::Has(a) | Self::Not(a) | Self::ToString(a) => vec![a],
            Self::Compare(_, a, b) | Self::In(a, b) => vec![a, b],
            Self::All(xs) | Self::Any(xs) | Self::Coalesce(xs) | Self::ToNumber(xs) => {
                xs.iter().collect()
            }
            Self::Match {
                input,
                branches,
                fallback,
            } => {
                let mut v: Vec<&Expr> = vec![input];
                v.extend(branches.iter().map(|b| &b.output));
                v.push(fallback);
                v
            }
            Self::Case { branches, fallback } => {
                let mut v: Vec<&Expr> = branches.iter().flat_map(|(c, o)| [c, o]).collect();
                v.push(fallback);
                v
            }
            Self::Interpolate { input, stops, .. } => {
                let mut v: Vec<&Expr> = vec![input];
                v.extend(stops.iter().map(|(_, o)| o));
                v
            }
            Self::Step { input, base, stops } => {
                let mut v: Vec<&Expr> = vec![input, base];
                v.extend(stops.iter().map(|(_, o)| o));
                v
            }
        }
    }

    fn any_node(&self, pred: &dyn Fn(&Expr) -> bool) -> bool {
        pred(self) || self.children().into_iter().any(|c| c.any_node(pred))
    }

    /// Visit every literal that can become this expression's result (match
    /// and case outputs, coalesce operands, step/interpolate outputs).
    /// Used to coerce string literals to colors in color-typed properties.
    pub(crate) fn visit_output_literals(
        &mut self,
        f: &mut dyn FnMut(&mut Value) -> Result<(), String>,
    ) -> Result<(), String> {
        match self {
            Self::Literal(v) => f(v),
            Self::Match {
                branches, fallback, ..
            } => {
                for b in branches {
                    b.output.visit_output_literals(f)?;
                }
                fallback.visit_output_literals(f)
            }
            Self::Case { branches, fallback } => {
                for (_, o) in branches {
                    o.visit_output_literals(f)?;
                }
                fallback.visit_output_literals(f)
            }
            Self::Coalesce(xs) => xs.iter_mut().try_for_each(|x| x.visit_output_literals(f)),
            Self::Interpolate { stops, .. } => stops
                .iter_mut()
                .try_for_each(|(_, o)| o.visit_output_literals(f)),
            Self::Step { base, stops, .. } => {
                base.visit_output_literals(f)?;
                stops
                    .iter_mut()
                    .try_for_each(|(_, o)| o.visit_output_literals(f))
            }
            _ => Ok(()),
        }
    }
}

/// An evaluation result. Literals and feature attributes are borrowed;
/// only `to-string` builds a new string.
pub(crate) enum Eval<'a> {
    Ref(ValueRef<'a>),
    String(String),
}

impl Eval<'_> {
    pub(crate) fn view(&self) -> ValueRef<'_> {
        match self {
            Self::Ref(v) => *v,
            Self::String(s) => ValueRef::String(s),
        }
    }

    pub(crate) fn into_value(self) -> Value {
        match self {
            Self::Ref(v) => v.to_value(),
            Self::String(s) => Value::String(s),
        }
    }
}

fn expect_string<'v>(op: &'static str, v: ValueRef<'v>) -> Result<&'v str, EvalError> {
    match v {
        ValueRef::String(s) => Ok(s),
        other => Err(type_error(op, "string", other)),
    }
}

/// The numeric input of `interpolate`/`step`, which must be finite.
fn finite_input(op: &'static str, v: ValueRef<'_>) -> Result<f64, EvalError> {
    let x = v.expect_number(op)?;
    if x.is_finite() {
        Ok(x)
    } else {
        Err(EvalError::NonFinite { op })
    }
}

/// MapLibre `to-number` of one operand: null is 0, booleans are 0/1 and
/// strings parse as decimal numbers after trimming (`""` is 0, like
/// JavaScript's `Number("")`); `None` if not convertible.
fn to_number(v: ValueRef<'_>) -> Option<f64> {
    match v {
        ValueRef::Number(n) => Some(n),
        ValueRef::Null => Some(0.0),
        ValueRef::Bool(b) => Some(f64::from(u8::from(b))),
        ValueRef::String(s) => {
            let t = s.trim();
            if t.is_empty() {
                Some(0.0)
            } else {
                t.parse::<f64>().ok().filter(|n| n.is_finite())
            }
        }
        ValueRef::Color(_) | ValueRef::Array(_) => None,
    }
}

fn compare(op: CompareOp, a: ValueRef<'_>, b: ValueRef<'_>) -> Result<bool, EvalError> {
    use std::cmp::Ordering;
    match op {
        // Differently-typed operands are simply unequal.
        CompareOp::Eq => Ok(a == b),
        CompareOp::Ne => Ok(a != b),
        _ => {
            let ord = match (a, b) {
                (ValueRef::Number(x), ValueRef::Number(y)) => x.partial_cmp(&y),
                (ValueRef::String(x), ValueRef::String(y)) => Some(x.cmp(y)),
                _ => {
                    return Err(EvalError::Compare {
                        op: op.name(),
                        left: a.type_name(),
                        right: b.type_name(),
                    });
                }
            };
            Ok(match (op, ord) {
                (_, None) => false,
                (CompareOp::Lt, Some(o)) => o == Ordering::Less,
                (CompareOp::Le, Some(o)) => o != Ordering::Greater,
                (CompareOp::Gt, Some(o)) => o == Ordering::Greater,
                (CompareOp::Ge, Some(o)) => o != Ordering::Less,
                _ => unreachable!("Eq/Ne handled above"),
            })
        }
    }
}

fn interpolate<'a>(
    interpolation: Interpolation,
    x: f64,
    stops: &'a [(f64, Expr)],
    ctx: &EvalContext<'a>,
) -> Result<Eval<'a>, EvalError> {
    let (first, last) = match (stops.first(), stops.last()) {
        (Some(f), Some(l)) => (f, l),
        _ => return Err(EvalError::NoStops { op: "interpolate" }),
    };
    if !x.is_finite() {
        return Err(EvalError::NonFinite { op: "interpolate" });
    }
    if x <= first.0 {
        return first.1.eval(ctx);
    }
    if x >= last.0 {
        return last.1.eval(ctx);
    }
    // `first.0 < x < last.0`, so at least one stop is `<= x` and one is
    // `> x`; the saturation only guards against unsorted programmatic stops.
    let i = stops
        .partition_point(|(s, _)| *s <= x)
        .saturating_sub(1)
        .min(stops.len() - 2);
    let ((x0, lo), (x1, hi)) = (&stops[i], &stops[i + 1]);
    let t = interpolation_factor(interpolation, x, *x0, *x1);
    let (lo, hi) = (lo.eval(ctx)?, hi.eval(ctx)?);
    Ok(Eval::Ref(match (lo.view(), hi.view()) {
        (ValueRef::Number(a), ValueRef::Number(b)) => ValueRef::Number(a + (b - a) * t),
        (
            lo @ (ValueRef::Color(_) | ValueRef::String(_)),
            hi @ (ValueRef::Color(_) | ValueRef::String(_)),
        ) => ValueRef::Color(lerp_color(lo.to_color()?, hi.to_color()?, t as f32)),
        (ValueRef::Number(_), other) | (other, _) => {
            return Err(type_error("interpolate", "number or color", other));
        }
    }))
}

/// How far `x` is from `x0` towards `x1`, in `[0, 1]`.
///
/// Exponential interpolation follows MapLibre,
/// `(b^(x-x0) - 1) / (b^(x1-x0) - 1)`. For large bases or spans both powers
/// overflow to infinity; the ratio then tends to `b^(x-x1)`, which is used
/// instead. Anything still not a number (an unsorted, programmatically
/// built stop list) maps to 0.
fn interpolation_factor(interpolation: Interpolation, x: f64, x0: f64, x1: f64) -> f64 {
    let t = match interpolation {
        Interpolation::Exponential(base) if (base - 1.0).abs() > f64::EPSILON => {
            let t = (base.powf(x - x0) - 1.0) / (base.powf(x1 - x0) - 1.0);
            if t.is_finite() { t } else { base.powf(x - x1) }
        }
        _ => (x - x0) / (x1 - x0),
    };
    if t.is_nan() { 0.0 } else { t.clamp(0.0, 1.0) }
}

/// Interpolate in premultiplied space, as MapLibre does.
fn lerp_color(a: Color, b: Color, t: f32) -> Color {
    let (pa, pb) = (a.premultiplied(), b.premultiplied());
    let m: [f32; 4] = std::array::from_fn(|i| pa[i] + (pb[i] - pa[i]) * t);
    let alpha = m[3];
    if alpha <= 0.0 {
        return Color::TRANSPARENT;
    }
    Color::rgba(m[0] / alpha, m[1] / alpha, m[2] / alpha, alpha)
}

fn legacy_value(json: &Json, path: &str) -> Result<Value, StyleError> {
    match json {
        Json::Null | Json::Bool(_) | Json::Number(_) | Json::String(_) => {
            Value::from_json(json, 0).map_err(|m| StyleError::invalid(path, m))
        }
        _ => Err(StyleError::invalid(
            path,
            "legacy filter values must be strings, numbers, booleans or null",
        )),
    }
}

fn parse_call(items: &[Json], path: &str, depth: usize) -> Result<Expr, StyleError> {
    let Some(Json::String(op)) = items.first() else {
        return Err(StyleError::invalid(
            path,
            "an expression array must start with an operator name",
        ));
    };
    let args = &items[1..];
    let arg = |i: usize| parse_expr(&args[i], &child_path(path, i + 1), depth + 1);
    let boxed = |i: usize| arg(i).map(Box::new);
    let all_args = || -> Result<Vec<Expr>, StyleError> { (0..args.len()).map(arg).collect() };
    let arity = |n: usize| -> Result<(), StyleError> {
        if args.len() == n {
            Ok(())
        } else {
            Err(StyleError::invalid(
                path,
                format!("`{op}` expects {n} argument(s), found {}", args.len()),
            ))
        }
    };

    match op.as_str() {
        "literal" => {
            arity(1)?;
            Value::from_json(&args[0], depth + 1)
                .map(Expr::Literal)
                .map_err(|m| StyleError::invalid(path, m))
        }
        "zoom" => {
            arity(0)?;
            Ok(Expr::Zoom)
        }
        "get" | "has" | "!has" => {
            if args.len() == 2 {
                return Err(StyleError::unsupported(
                    path,
                    "expression form",
                    format!("{op} with an object argument"),
                ));
            }
            arity(1)?;
            let key = boxed(0)?;
            Ok(match op.as_str() {
                "get" => Expr::Get(key),
                "has" => Expr::Has(key),
                _ => Expr::Not(Box::new(Expr::Has(key))),
            })
        }
        "!" => {
            arity(1)?;
            Ok(Expr::Not(boxed(0)?))
        }
        "==" | "!=" | "<" | "<=" | ">" | ">=" => {
            if args.len() == 3 {
                return Err(StyleError::unsupported(
                    path,
                    "expression form",
                    format!("{op} with a collator"),
                ));
            }
            arity(2)?;
            let cmp = CompareOp::from_name(op).expect("listed above");
            Ok(Expr::Compare(cmp, boxed(0)?, boxed(1)?))
        }
        "all" => Ok(Expr::All(all_args()?)),
        "any" => Ok(Expr::Any(all_args()?)),
        "in" | "!in" => {
            arity(2)?;
            let test = Expr::In(boxed(0)?, boxed(1)?);
            Ok(if op == "in" {
                test
            } else {
                Expr::Not(Box::new(test))
            })
        }
        "match" => parse_match(args, path, depth),
        "case" => {
            if args.len() < 3 || args.len().is_multiple_of(2) {
                return Err(StyleError::invalid(
                    path,
                    "`case` expects condition/output pairs followed by a fallback",
                ));
            }
            let mut branches = Vec::new();
            for pair in 0..(args.len() - 1) / 2 {
                branches.push((arg(2 * pair)?, arg(2 * pair + 1)?));
            }
            Ok(Expr::Case {
                branches,
                fallback: boxed(args.len() - 1)?,
            })
        }
        "coalesce" => {
            if args.is_empty() {
                return Err(StyleError::invalid(
                    path,
                    "`coalesce` expects at least one argument",
                ));
            }
            Ok(Expr::Coalesce(all_args()?))
        }
        "interpolate" => parse_interpolate(args, path, depth),
        "step" => parse_step(args, path, depth),
        "to-string" => {
            arity(1)?;
            Ok(Expr::ToString(boxed(0)?))
        }
        "to-number" => {
            if args.is_empty() {
                return Err(StyleError::invalid(
                    path,
                    "`to-number` expects at least one argument",
                ));
            }
            Ok(Expr::ToNumber(all_args()?))
        }
        other => Err(StyleError::unsupported(path, "expression operator", other)),
    }
}

fn parse_match(args: &[Json], path: &str, depth: usize) -> Result<Expr, StyleError> {
    if args.len() < 4 || !args.len().is_multiple_of(2) {
        return Err(StyleError::invalid(
            path,
            "`match` expects an input, label/output pairs and a fallback",
        ));
    }
    let input = Box::new(parse_expr(&args[0], &child_path(path, 1), depth + 1)?);
    let mut branches = Vec::new();
    let mut seen: Vec<Value> = Vec::new();
    let mut kind: Option<&'static str> = None;
    for pair in 0..(args.len() - 2) / 2 {
        let label_path = child_path(path, 2 * pair + 2);
        let label_json = &args[1 + 2 * pair];
        let scalars: Vec<&Json> = match label_json {
            Json::Array(a) if !a.is_empty() => a.iter().collect(),
            Json::Array(_) => {
                return Err(StyleError::invalid(&label_path, "empty label array"));
            }
            single => vec![single],
        };
        let mut labels = Vec::new();
        for s in scalars {
            let v = match s {
                Json::String(_) | Json::Number(_) => Value::from_json(s, depth + 1)
                    .map_err(|m| StyleError::invalid(&label_path, m))?,
                _ => {
                    return Err(StyleError::invalid(
                        &label_path,
                        "match labels must be strings or numbers",
                    ));
                }
            };
            let k = v.type_name();
            if *kind.get_or_insert(k) != k {
                return Err(StyleError::invalid(
                    &label_path,
                    "match labels must all be strings or all be numbers",
                ));
            }
            if seen.contains(&v) {
                return Err(StyleError::invalid(&label_path, "duplicate match label"));
            }
            seen.push(v.clone());
            labels.push(v);
        }
        let output = parse_expr(
            &args[2 + 2 * pair],
            &child_path(path, 2 * pair + 3),
            depth + 1,
        )?;
        branches.push(MatchBranch { labels, output });
    }
    let fallback = Box::new(parse_expr(
        &args[args.len() - 1],
        &child_path(path, args.len()),
        depth + 1,
    )?);
    Ok(Expr::Match {
        input,
        branches,
        fallback,
    })
}

fn parse_stops(
    args: &[Json],
    first: usize,
    path: &str,
    depth: usize,
) -> Result<Vec<(f64, Expr)>, StyleError> {
    let mut stops: Vec<(f64, Expr)> = Vec::new();
    for (n, pair) in args[first..].chunks(2).enumerate() {
        let at = first + 2 * n;
        let stop = pair[0].as_f64().ok_or_else(|| {
            StyleError::invalid(
                &child_path(path, at + 1),
                "stop input must be a number literal",
            )
        })?;
        if stops.last().is_some_and(|(prev, _)| stop <= *prev) {
            return Err(StyleError::invalid(
                &child_path(path, at + 1),
                "stop inputs must be in strictly ascending order",
            ));
        }
        let out = parse_expr(&pair[1], &child_path(path, at + 2), depth + 1)?;
        stops.push((stop, out));
    }
    Ok(stops)
}

fn parse_interpolate(args: &[Json], path: &str, depth: usize) -> Result<Expr, StyleError> {
    if args.len() < 4 || !args.len().is_multiple_of(2) {
        return Err(StyleError::invalid(
            path,
            "`interpolate` expects an interpolation type, an input and input/output stops",
        ));
    }
    let kind_path = child_path(path, 1);
    let Json::Array(kind) = &args[0] else {
        return Err(StyleError::invalid(
            &kind_path,
            "expected an interpolation type",
        ));
    };
    let interpolation = match kind.first().and_then(Json::as_str) {
        Some("linear") if kind.len() == 1 => Interpolation::Linear,
        Some("exponential") if kind.len() == 2 => {
            let base = kind[1]
                .as_f64()
                .filter(|b| *b > 0.0 && b.is_finite())
                .ok_or_else(|| {
                    StyleError::invalid(&kind_path, "exponential base must be a positive number")
                })?;
            Interpolation::Exponential(base)
        }
        Some("cubic-bezier") => {
            return Err(StyleError::unsupported(
                &kind_path,
                "interpolation type",
                "cubic-bezier",
            ));
        }
        Some("linear" | "exponential") => {
            return Err(StyleError::invalid(
                &kind_path,
                "malformed interpolation type",
            ));
        }
        Some(other) => {
            return Err(StyleError::unsupported(
                &kind_path,
                "interpolation type",
                other,
            ));
        }
        None => {
            return Err(StyleError::invalid(
                &kind_path,
                "expected an interpolation type",
            ));
        }
    };
    let input = Box::new(parse_expr(&args[1], &child_path(path, 2), depth + 1)?);
    let stops = parse_stops(args, 2, path, depth)?;
    Ok(Expr::Interpolate {
        interpolation,
        input,
        stops,
    })
}

fn parse_step(args: &[Json], path: &str, depth: usize) -> Result<Expr, StyleError> {
    if args.len() < 2 || !args.len().is_multiple_of(2) {
        return Err(StyleError::invalid(
            path,
            "`step` expects an input, a base output and input/output stops",
        ));
    }
    let input = Box::new(parse_expr(&args[0], &child_path(path, 1), depth + 1)?);
    let base = Box::new(parse_expr(&args[1], &child_path(path, 2), depth + 1)?);
    let stops = parse_stops(args, 2, path, depth)?;
    Ok(Expr::Step { input, base, stops })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn props() -> Vec<(&'static str, &'static str)> {
        vec![("class", "primary"), ("name", "Main St"), ("lanes", "4")]
    }

    fn eval_at(expr: Json, zoom: f64) -> Result<Value, EvalError> {
        let p = props();
        let e = Expr::parse(&expr).unwrap_or_else(|err| panic!("{expr}: {err}"));
        e.evaluate(&EvalContext::new(zoom, &p))
    }

    fn eval(expr: Json) -> Value {
        eval_at(expr, 10.0).unwrap_or_else(|e| panic!("eval failed: {e}"))
    }

    fn filter(f: Json) -> bool {
        let p = props();
        Expr::parse_filter(&f, "filter")
            .unwrap_or_else(|e| panic!("{f}: {e}"))
            .evaluate_bool(&EvalContext::new(10.0, &p))
    }

    #[test]
    fn literals() {
        assert_eq!(eval(json!(3)), Value::Number(3.0));
        assert_eq!(eval(json!("x")), Value::String("x".into()));
        assert_eq!(eval(json!(true)), Value::Bool(true));
        assert_eq!(eval(json!(null)), Value::Null);
        assert_eq!(
            eval(json!(["literal", [1, 2]])),
            Value::Array(vec![Value::Number(1.0), Value::Number(2.0)])
        );
    }

    #[test]
    fn get_has_and_negation() {
        assert_eq!(
            eval(json!(["get", "class"])),
            Value::String("primary".into())
        );
        assert_eq!(eval(json!(["get", "missing"])), Value::Null);
        assert_eq!(eval(json!(["has", "name"])), Value::Bool(true));
        assert_eq!(eval(json!(["has", "ref"])), Value::Bool(false));
        assert_eq!(eval(json!(["!has", "ref"])), Value::Bool(true));
        assert_eq!(eval(json!(["!", ["has", "ref"]])), Value::Bool(true));
    }

    #[test]
    fn comparisons() {
        assert_eq!(
            eval(json!(["==", ["get", "class"], "primary"])),
            Value::Bool(true)
        );
        assert_eq!(
            eval(json!(["!=", ["get", "class"], "primary"])),
            Value::Bool(false)
        );
        assert_eq!(eval(json!(["<", 1, 2])), Value::Bool(true));
        assert_eq!(eval(json!(["<=", 2, 2])), Value::Bool(true));
        assert_eq!(eval(json!([">", 1, 2])), Value::Bool(false));
        assert_eq!(eval(json!([">=", 2, 2])), Value::Bool(true));
        assert_eq!(eval(json!(["<", "a", "b"])), Value::Bool(true));
        // Mismatched types: equality is false, ordering is an error.
        assert_eq!(eval(json!(["==", 1, "1"])), Value::Bool(false));
        assert!(eval_at(json!(["<", ["get", "lanes"], 5]), 0.0).is_err());
        assert_eq!(
            eval(json!(["<", ["to-number", ["get", "lanes"]], 5])),
            Value::Bool(true)
        );
    }

    #[test]
    fn all_any() {
        assert_eq!(
            eval(json!(["all", true, ["has", "name"]])),
            Value::Bool(true)
        );
        assert_eq!(eval(json!(["all", true, false])), Value::Bool(false));
        assert_eq!(
            eval(json!(["any", false, ["has", "name"]])),
            Value::Bool(true)
        );
        assert_eq!(eval(json!(["any", false, false])), Value::Bool(false));
        assert_eq!(eval(json!(["all"])), Value::Bool(true));
        assert_eq!(eval(json!(["any"])), Value::Bool(false));
    }

    #[test]
    fn in_expression_form() {
        assert_eq!(
            eval(json!([
                "in",
                ["get", "class"],
                ["literal", ["primary", "trunk"]]
            ])),
            Value::Bool(true)
        );
        assert_eq!(
            eval(json!(["in", ["get", "class"], ["literal", ["service"]]])),
            Value::Bool(false)
        );
        assert_eq!(
            eval(json!(["in", "Main", ["get", "name"]])),
            Value::Bool(true)
        );
        assert_eq!(
            eval(json!(["!in", ["get", "class"], ["literal", ["service"]]])),
            Value::Bool(true)
        );
    }

    #[test]
    fn match_expression() {
        let e = json!([
            "match",
            ["get", "class"],
            "motorway",
            6,
            ["primary", "trunk"],
            4,
            1
        ]);
        assert_eq!(eval(e), Value::Number(4.0));
        let e = json!(["match", ["get", "missing"], "a", 1, 2]);
        assert_eq!(eval(e), Value::Number(2.0));
        let e = json!(["match", ["to-number", ["get", "lanes"]], 4, "four", "other"]);
        assert_eq!(eval(e), Value::String("four".into()));
    }

    #[test]
    fn match_rejects_duplicate_and_mixed_labels() {
        assert!(Expr::parse(&json!(["match", ["get", "c"], "a", 1, "a", 2, 3])).is_err());
        assert!(Expr::parse(&json!(["match", ["get", "c"], "a", 1, 5, 2, 3])).is_err());
    }

    #[test]
    fn case_expression() {
        let e = json!([
            "case",
            ["==", ["get", "class"], "x"],
            1,
            ["has", "name"],
            2,
            3
        ]);
        assert_eq!(eval(e), Value::Number(2.0));
        let e = json!(["case", false, 1, 3]);
        assert_eq!(eval(e), Value::Number(3.0));
    }

    #[test]
    fn coalesce_expression() {
        assert_eq!(
            eval(json!(["coalesce", ["get", "ref"], ["get", "name"]])),
            Value::String("Main St".into())
        );
        assert_eq!(
            eval(json!(["coalesce", ["get", "ref"], "fallback"])),
            Value::String("fallback".into())
        );
    }

    #[test]
    fn zoom_expression() {
        assert_eq!(eval_at(json!(["zoom"]), 7.5).unwrap(), Value::Number(7.5));
    }

    #[test]
    fn interpolate_linear() {
        let e = json!(["interpolate", ["linear"], ["zoom"], 5, 1, 15, 11]);
        assert_eq!(eval_at(e.clone(), 10.0).unwrap(), Value::Number(6.0));
        assert_eq!(eval_at(e.clone(), 0.0).unwrap(), Value::Number(1.0));
        assert_eq!(eval_at(e, 20.0).unwrap(), Value::Number(11.0));
    }

    #[test]
    fn interpolate_exponential() {
        let e = json!(["interpolate", ["exponential", 2], ["zoom"], 0, 0, 2, 3]);
        // t = (2^1 - 1) / (2^2 - 1) = 1/3
        let Value::Number(n) = eval_at(e, 1.0).unwrap() else {
            panic!("number expected")
        };
        assert!((n - 1.0).abs() < 1e-9, "{n}");
    }

    #[test]
    fn interpolate_colors() {
        let mut e = Expr::parse(&json!([
            "interpolate",
            ["linear"],
            ["zoom"],
            0,
            "#000000",
            10,
            "#ffffff"
        ]))
        .unwrap();
        e.visit_output_literals(&mut |v| {
            *v = Value::Color(v.to_color().map_err(|e| e.to_string())?);
            Ok(())
        })
        .unwrap();
        let v = e.evaluate(&EvalContext::at_zoom(5.0)).unwrap();
        let Value::Color(c) = v else {
            panic!("color expected")
        };
        assert!((c.r - 0.5).abs() < 1e-6 && (c.g - 0.5).abs() < 1e-6);
        // String outputs are coerced at evaluation time as well.
        let e = Expr::parse(&json!([
            "interpolate",
            ["linear"],
            ["zoom"],
            0,
            "#000000",
            10,
            "#ffffff"
        ]))
        .unwrap();
        assert!(matches!(
            e.evaluate(&EvalContext::at_zoom(5.0)),
            Ok(Value::Color(_))
        ));
    }

    #[test]
    fn step_expression() {
        let e = json!(["step", ["zoom"], 1, 5, 2, 10, 3]);
        assert_eq!(eval_at(e.clone(), 4.0).unwrap(), Value::Number(1.0));
        assert_eq!(eval_at(e.clone(), 5.0).unwrap(), Value::Number(2.0));
        assert_eq!(eval_at(e, 12.0).unwrap(), Value::Number(3.0));
    }

    #[test]
    fn to_string_and_to_number() {
        assert_eq!(eval(json!(["to-string", 3])), Value::String("3".into()));
        assert_eq!(eval(json!(["to-string", 2.5])), Value::String("2.5".into()));
        assert_eq!(
            eval(json!(["to-string", true])),
            Value::String("true".into())
        );
        assert_eq!(
            eval(json!(["to-string", ["get", "missing"]])),
            Value::String(String::new())
        );
        assert_eq!(
            eval(json!(["to-number", ["get", "lanes"]])),
            Value::Number(4.0)
        );
        assert_eq!(
            eval(json!(["to-number", ["get", "name"], 7])),
            Value::Number(7.0)
        );
        assert!(eval_at(json!(["to-number", ["get", "name"]]), 0.0).is_err());
    }

    #[test]
    fn legacy_filters() {
        assert!(filter(json!(["==", "class", "primary"])));
        assert!(!filter(json!(["!=", "class", "primary"])));
        assert!(filter(json!(["in", "class", "primary", "trunk"])));
        assert!(!filter(json!(["in", "class", "service"])));
        assert!(filter(json!(["!in", "class", "service"])));
        assert!(filter(json!(["has", "name"])));
        assert!(filter(json!(["!has", "ref"])));
        assert!(filter(json!([
            "all",
            ["has", "name"],
            ["==", "class", "primary"]
        ])));
        assert!(filter(json!([
            "any",
            ["has", "ref"],
            ["==", "class", "primary"]
        ])));
        assert!(filter(json!([
            "none",
            ["has", "ref"],
            ["==", "class", "service"]
        ])));
        assert!(!filter(json!(["none", ["has", "name"]])));
        // Legacy ordering compares like with like; attributes are strings.
        assert!(filter(json!([">", "class", "a"])));
    }

    #[test]
    fn expression_filters_and_mixed_nesting() {
        assert!(filter(json!(["==", ["get", "class"], "primary"])));
        assert!(filter(json!([
            "all",
            ["has", "name"],
            ["==", ["get", "class"], "primary"]
        ])));
        assert!(filter(json!([
            "in",
            ["get", "class"],
            ["literal", ["primary"]]
        ])));
        assert!(filter(json!(true)));
    }

    #[test]
    fn legacy_and_expression_forms_normalise_identically() {
        let legacy = Expr::parse_filter(&json!(["==", "class", "x"]), "f").unwrap();
        let modern = Expr::parse_filter(&json!(["==", ["get", "class"], "x"]), "f").unwrap();
        assert_eq!(legacy, modern);
        let a = Expr::parse_filter(&json!(["!has", "name"]), "f").unwrap();
        let b = Expr::parse_filter(&json!(["!", ["has", "name"]]), "f").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn unsupported_constructs_are_named() {
        for (expr, name) in [
            (json!(["concat", "a", "b"]), "concat"),
            (json!(["+", 1, 2]), "+"),
            (json!(["format", "a"]), "format"),
            (json!(["let", "a", 1, ["var", "a"]]), "let"),
            (
                json!([
                    "interpolate",
                    ["cubic-bezier", 0, 0, 1, 1],
                    ["zoom"],
                    0,
                    0,
                    1,
                    1
                ]),
                "cubic-bezier",
            ),
            (
                json!([
                    "interpolate-hcl",
                    ["linear"],
                    ["zoom"],
                    0,
                    "#000",
                    1,
                    "#fff"
                ]),
                "interpolate-hcl",
            ),
            (json!(["nonsense"]), "nonsense"),
        ] {
            let err = Expr::parse(&expr).expect_err(name);
            assert_eq!(err.construct(), Some(name), "{expr} -> {err}");
        }
        let err = Expr::parse_filter(&json!(["==", "$type", "Polygon"]), "f").unwrap_err();
        assert_eq!(err.construct(), Some("$type"));
        let err = Expr::parse(&json!({"a": 1})).unwrap_err();
        assert_eq!(err.construct(), Some("object literal"));
    }

    #[test]
    fn malformed_expressions_are_invalid_not_unsupported() {
        for e in [
            json!(["get"]),
            json!(["!", 1, 2]),
            json!(["match", ["get", "c"], "a", 1]),
            json!(["case", true, 1]),
            json!(["interpolate", ["linear"], ["zoom"], 5, 1, 3, 2]),
            json!(["interpolate", ["linear"], ["zoom"], 5]),
            json!(["step", ["zoom"]]),
            json!([]),
            json!([1, 2]),
        ] {
            let err = Expr::parse(&e).expect_err(&e.to_string());
            assert!(matches!(err, StyleError::Invalid { .. }), "{e} -> {err}");
        }
    }

    #[test]
    fn json_round_trip() {
        for e in [
            json!(["match", ["get", "class"], "a", 1, ["b", "c"], 2, 3]),
            json!(["case", ["has", "name"], "x", "y"]),
            json!([
                "interpolate",
                ["exponential", 1.5],
                ["zoom"],
                5,
                1,
                15,
                11.5
            ]),
            json!(["step", ["zoom"], 1, 5, 2]),
            json!([
                "all",
                ["!", ["has", "a"]],
                ["in", ["get", "c"], ["literal", ["a", "b"]]]
            ]),
            json!([
                "coalesce",
                ["to-string", ["get", "a"]],
                ["to-number", ["get", "b"], 1]
            ]),
            json!(["<=", ["zoom"], 12]),
        ] {
            let parsed = Expr::parse(&e).unwrap();
            assert_eq!(parsed.to_json(), e);
            assert_eq!(Expr::parse(&parsed.to_json()).unwrap(), parsed);
        }
    }

    #[test]
    fn non_finite_inputs_are_errors_not_panics() {
        let interp = json!(["interpolate", ["linear"], ["zoom"], 0, 1, 5, 2, 10, 3]);
        let step = json!(["step", ["zoom"], 1, 5, 2]);
        for zoom in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(
                eval_at(interp.clone(), zoom),
                Err(EvalError::NonFinite { op: "interpolate" })
            );
            assert_eq!(
                eval_at(step.clone(), zoom),
                Err(EvalError::NonFinite { op: "step" })
            );
        }
        // A failing property expression yields the property default.
        let p = crate::Property::<f64>::parse(&interp, "p").unwrap();
        assert_eq!(p.evaluate(&EvalContext::at_zoom(f64::NAN), &7.0), 7.0);
    }

    #[test]
    fn exponential_overflow_stays_finite_and_ordered() {
        // base^(x1 - x0) overflows to infinity: inf / inf would be NaN.
        let e = json!([
            "interpolate",
            ["exponential", 1.0e10],
            ["zoom"],
            0,
            0,
            1000,
            10
        ]);
        let at = |z: f64| match eval_at(e.clone(), z).unwrap() {
            Value::Number(n) => n,
            other => panic!("{other:?}"),
        };
        let (a, b, c) = (at(1.0), at(999.95), at(999.99));
        assert!(
            [a, b, c]
                .iter()
                .all(|v| v.is_finite() && (0.0..=10.0).contains(v))
        );
        assert!(a <= b && b <= c, "{a} {b} {c}");
        // Colors never become NaN either, so rendering cannot fail on them.
        let mut colors = Expr::parse(&json!([
            "interpolate",
            ["exponential", 1.0e10],
            ["zoom"],
            0,
            "#000000",
            1000,
            "#ffffff"
        ]))
        .unwrap();
        colors
            .visit_output_literals(&mut |v| {
                *v = Value::Color(v.to_color().map_err(|e| e.to_string())?);
                Ok(())
            })
            .unwrap();
        let Value::Color(c) = colors.evaluate(&EvalContext::at_zoom(500.0)).unwrap() else {
            panic!("color expected")
        };
        assert!([c.r, c.g, c.b, c.a].iter().all(|v| v.is_finite()), "{c:?}");
        // A NaN width from an infinite output falls back to the default.
        let p = crate::Property::<f64>::parse(
            &json!(["interpolate", ["linear"], ["zoom"], 0, 0, 10, 1e308]),
            "p",
        )
        .unwrap();
        let mut inf = p.clone();
        if let crate::Property::Expr(Expr::Interpolate { stops, .. }) = &mut inf {
            stops[1].1 = Expr::number(f64::INFINITY);
            stops[0].1 = Expr::number(f64::NEG_INFINITY);
        }
        assert_eq!(inf.evaluate(&EvalContext::at_zoom(5.0), &1.5), 1.5);
    }

    /// `leaf` wrapped `depth` times as `[op, ...]` (or `[...]` for an empty
    /// `op`). Built directly: `json!` would re-serialise the inner value at
    /// every level, recursively.
    fn nested(depth: usize, leaf: Json, op: &str) -> Json {
        (0..depth).fold(leaf, |acc, _| {
            Json::Array(if op.is_empty() {
                vec![acc]
            } else {
                vec![Json::from(op), acc]
            })
        })
    }

    #[test]
    fn nesting_depth_is_bounded() {
        // `depth` wrappers put the leaf `depth` levels below the root.
        let ok = nested(MAX_EXPRESSION_DEPTH, json!(true), "!");
        let e = Expr::parse(&ok).expect("at the limit");
        assert!(e.evaluate(&EvalContext::at_zoom(0.0)).is_ok());
        let deep = nested(MAX_EXPRESSION_DEPTH + 1, json!(true), "!");
        assert!(matches!(
            Expr::parse(&deep),
            Err(StyleError::Invalid { .. })
        ));
        // Legacy filters and nested literal arrays count too.
        let deep_filter = nested(MAX_EXPRESSION_DEPTH + 1, json!(["has", "a"]), "any");
        assert!(matches!(
            Expr::parse_filter(&deep_filter, "f"),
            Err(StyleError::Invalid { .. })
        ));
        let deep_literal = Json::Array(vec![
            Json::from("literal"),
            nested(MAX_EXPRESSION_DEPTH, json!(1), ""),
        ]);
        assert!(matches!(
            Expr::parse(&deep_literal),
            Err(StyleError::Invalid { .. })
        ));
        // Far deeper than any stack could recurse through: still an error.
        let huge = nested(100_000, json!(true), "!");
        assert!(Expr::parse(&huge).is_err());
        // serde_json's own drop of `huge` recurses; leak it so this test
        // only exercises the expression parser.
        std::mem::forget(huge);
    }

    #[test]
    fn to_number_and_coalesce_follow_maplibre() {
        // `Number("")` is 0 in MapLibre.
        assert_eq!(eval(json!(["to-number", ""])), Value::Number(0.0));
        assert_eq!(eval(json!(["to-number", "  "])), Value::Number(0.0));
        assert_eq!(eval(json!(["to-number", " 2.5 "])), Value::Number(2.5));
        // An operand that errors fails the expression instead of being
        // skipped.
        let failing = json!(["<", 1, "a"]);
        assert!(matches!(
            eval_at(json!(["to-number", failing.clone(), 5]), 0.0),
            Err(EvalError::Compare { .. })
        ));
        assert!(matches!(
            eval_at(json!(["coalesce", failing, 5]), 0.0),
            Err(EvalError::Compare { .. })
        ));
        assert_eq!(
            eval_at(json!(["to-number", ["get", "name"]]), 0.0),
            Err(EvalError::NotANumber { found: "string" })
        );
    }

    #[test]
    fn every_listed_operator_is_parsed() {
        for op in SUPPORTED_OPS {
            if let Err(e) = Expr::parse(&json!([op])) {
                assert!(
                    matches!(e, StyleError::Invalid { .. }),
                    "`{op}` is listed as supported but rejected: {e}"
                );
            }
        }
        // `none` is legacy filter syntax only.
        assert!(Expr::parse_filter(&json!(["none"]), "f").is_ok());
        assert!(!is_expression(&json!(["none"])));
    }

    #[test]
    fn legacy_in_filters_serialise_to_valid_maplibre() {
        for (legacy, matching, other) in [
            (
                json!(["in", "x", 1, "a"]),
                vec![("x", "a")],
                vec![("x", "b")],
            ),
            (
                json!(["in", "x", "a", "a"]),
                vec![("x", "a")],
                vec![("x", "b")],
            ),
            (json!(["in", "x", true]), vec![], vec![("x", "true")]),
            (
                json!(["!in", "x", 1, "a", "a"]),
                vec![("x", "b")],
                vec![("x", "a")],
            ),
        ] {
            let parsed = Expr::parse_filter(&legacy, "f").unwrap();
            let out = parsed.to_json();
            // The output is valid (non-legacy) expression syntax that
            // parses back to the same tree.
            let reparsed = Expr::parse(&out).unwrap_or_else(|e| panic!("{legacy} -> {out}: {e}"));
            assert_eq!(reparsed, parsed, "{legacy} -> {out}");
            if !matching.is_empty() {
                assert!(
                    parsed.evaluate_bool(&EvalContext::new(0.0, &matching)),
                    "{legacy}"
                );
            }
            assert!(
                !parsed.evaluate_bool(&EvalContext::new(0.0, &other)),
                "{legacy}"
            );
        }
        // Homogeneous labels still use `match`.
        let p = Expr::parse_filter(&json!(["in", "x", "a", "b", "a"]), "f").unwrap();
        assert_eq!(
            p.to_json(),
            json!(["match", ["get", "x"], ["a", "b"], true, false])
        );
    }

    #[test]
    fn dependency_analysis() {
        let p = |j: Json| Expr::parse(&j).unwrap();
        assert!(p(json!(["get", "a"])).depends_on_feature());
        assert!(!p(json!(["interpolate", ["linear"], ["zoom"], 0, 1, 1, 2])).depends_on_feature());
        assert!(p(json!(["interpolate", ["linear"], ["zoom"], 0, 1, 1, 2])).depends_on_zoom());
        assert!(!p(json!(["match", ["get", "a"], "x", 1, 2])).depends_on_zoom());
    }
}
