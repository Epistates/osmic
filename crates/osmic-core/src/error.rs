use thiserror::Error;

#[derive(Error, Debug)]
#[non_exhaustive]
pub enum OsmicError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("PBF parsing error: {0}")]
    Pbf(String),

    #[error("Projection error: {0}")]
    Projection(String),

    #[error("Tile error: {0}")]
    Tile(String),

    #[error("Index error: {0}")]
    Index(String),

    #[error("Render error: {0}")]
    Render(String),

    #[error("Style error: {0}")]
    Style(String),

    #[error("Plugin error: {0}")]
    Plugin(String),

    #[error("{0}")]
    Other(String),
}

pub type OsmicResult<T> = Result<T, OsmicError>;
