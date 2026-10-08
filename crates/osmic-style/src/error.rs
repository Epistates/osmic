//! Style parse and evaluation errors.

use std::fmt;

/// Why a style document could not be turned into a [`crate::Style`].
///
/// Every error carries the JSON path of the offending element so that
/// failures in large styles are locatable. Constructs that MapLibre defines
/// but this crate does not implement are reported as
/// [`StyleError::Unsupported`] — they are never silently ignored.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum StyleError {
    /// The document is not valid JSON.
    #[error("invalid JSON: {0}")]
    Json(String),
    /// A valid MapLibre construct outside the supported subset.
    #[error("{path}: unsupported {kind} `{construct}`")]
    Unsupported {
        /// JSON path of the element.
        path: String,
        /// What sort of thing it is (`expression`, `layer type`, ...).
        kind: &'static str,
        /// The construct's name as written in the style.
        construct: String,
    },
    /// The element is malformed.
    #[error("{path}: {message}")]
    Invalid {
        /// JSON path of the element.
        path: String,
        /// What is wrong with it.
        message: String,
    },
}

impl StyleError {
    pub(crate) fn invalid(path: &str, message: impl fmt::Display) -> Self {
        Self::Invalid {
            path: path.to_string(),
            message: message.to_string(),
        }
    }

    pub(crate) fn unsupported(
        path: &str,
        kind: &'static str,
        construct: impl fmt::Display,
    ) -> Self {
        Self::Unsupported {
            path: path.to_string(),
            kind,
            construct: construct.to_string(),
        }
    }

    /// The unsupported construct's name, if this is an
    /// [`StyleError::Unsupported`] error.
    pub fn construct(&self) -> Option<&str> {
        match self {
            Self::Unsupported { construct, .. } => Some(construct),
            _ => None,
        }
    }
}

/// An expression failed at evaluation time (type mismatch, missing operand).
///
/// Renderers treat a failed filter as "feature not matched" and a failed
/// paint/layout property as "use the property's default", mirroring
/// MapLibre.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct EvalError(pub String);

impl EvalError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}
