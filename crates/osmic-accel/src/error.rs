use std::time::Duration;

use thiserror::Error;

/// Errors returned by `osmic-accel`.
///
/// The enum is `#[non_exhaustive]`; always include a wildcard arm when
/// matching.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum AccelError {
    /// A Metal device exists but could not be set up (for example no
    /// command queue could be created).
    #[error("Metal initialization failed: {0}")]
    MetalInit(String),

    /// A GPU buffer could not be created (too large, out of memory, ...).
    #[error("Buffer creation failed: {0}")]
    BufferCreation(String),

    /// The embedded shader library could not be loaded, or its kernel could
    /// not be found or turned into a pipeline.
    #[error("Shader compilation failed: {0}")]
    ShaderCompilation(String),

    /// The GPU reported an error while executing the command buffer, or wrote
    /// results that failed validation.
    #[error("Kernel execution failed: {0}")]
    ExecutionFailed(String),

    /// The GPU watchdog aborted the command buffer.
    #[error("GPU timeout after {0:?}")]
    GpuTimeout(Duration),

    /// GPU acceleration is not available: non-Apple platform, the crate was
    /// built without the Metal toolchain (no embedded metallib), or no Metal
    /// device exists. The CPU path ([`crate::clip_batch_cpu`]) always works.
    #[error("GPU acceleration is not available")]
    NotAvailable,

    /// A work item or option is invalid (non-finite coordinate, zoom or tile
    /// index out of range, ...).
    #[error("Invalid input: {0}")]
    InvalidInput(String),
}

/// Result alias for this crate.
pub type AccelResult<T> = Result<T, AccelError>;
