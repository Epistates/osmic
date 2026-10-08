//! Metal backend. Only compiled on macOS when `build.rs` produced a metallib
//! (`cfg(osmic_metallib)`); see the crate docs.

pub(crate) mod accelerator;
pub(crate) mod batch;
pub(crate) mod buffer;
pub(crate) mod context;
pub(crate) mod types;

use objc2::runtime::ProtocolObject;
use objc2_foundation::NSError;
use objc2_metal::{
    MTLBuffer, MTLCommandBuffer, MTLCommandQueue, MTLComputePipelineState, MTLDevice,
};

/// The Metal objects this backend uses. Metal hands out protocol objects
/// (`id<MTLDevice>` etc.); owned handles are `Retained<...>` of these.
pub(crate) type Device = ProtocolObject<dyn MTLDevice>;
pub(crate) type Buffer = ProtocolObject<dyn MTLBuffer>;
pub(crate) type CommandQueue = ProtocolObject<dyn MTLCommandQueue>;
pub(crate) type CommandBuffer = ProtocolObject<dyn MTLCommandBuffer>;
pub(crate) type ComputePipelineState = ProtocolObject<dyn MTLComputePipelineState>;

/// Human-readable text of an `NSError` returned by Metal.
pub(crate) fn error_description(error: &NSError) -> String {
    error.localizedDescription().to_string()
}
