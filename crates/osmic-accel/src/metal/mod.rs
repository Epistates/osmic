//! Metal backend. Only compiled on macOS when `build.rs` produced a metallib
//! (`cfg(osmic_metallib)`); see the crate docs.

pub(crate) mod accelerator;
pub(crate) mod batch;
pub(crate) mod buffer;
pub(crate) mod context;
pub(crate) mod types;
