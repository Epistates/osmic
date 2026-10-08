use std::sync::{Arc, OnceLock};

use dispatch2::DispatchData;
use objc2::rc::{Retained, autoreleasepool};
use objc2_foundation::NSString;
use objc2_metal::{MTLComputePipelineState, MTLCreateSystemDefaultDevice, MTLDevice, MTLLibrary};
use tracing::info;

use crate::error::{AccelError, AccelResult};

use super::{CommandQueue, ComputePipelineState, Device, error_description};

/// Compiled shader library embedded by `build.rs` (this module only exists
/// when the `osmic_metallib` cfg is set, i.e. the metallib was produced).
static METALLIB: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/osmic_geometry.metallib"));

const CLIP_KERNEL: &str = "clip_units";

/// Threadgroup width as a multiple of the pipeline's SIMD width.
const SIMD_GROUPS_PER_THREADGROUP: usize = 4;

/// Process-wide Metal state: device, queue and the compiled clip pipeline.
///
/// `objc2-metal` declares `MTLDevice`, `MTLCommandQueue` and
/// `MTLComputePipelineState` as `Send + Sync` (Apple documents them as
/// thread-safe), hence the struct is `Send + Sync` without any `unsafe impl`.
pub(crate) struct MetalContext {
    device: Retained<Device>,
    command_queue: Retained<CommandQueue>,
    clip_pipeline: Retained<ComputePipelineState>,
    threads_per_group: usize,
}

/// Initialisation result is cached for the process lifetime; the error is
/// kept as text because `AccelError` is not `Clone`.
static CONTEXT: OnceLock<Result<Arc<MetalContext>, String>> = OnceLock::new();

impl MetalContext {
    /// Shared context, initialised on first use.
    pub(crate) fn get() -> AccelResult<Arc<MetalContext>> {
        match CONTEXT.get_or_init(|| Self::init().map(Arc::new).map_err(|e| e.to_string())) {
            Ok(ctx) => Ok(Arc::clone(ctx)),
            Err(msg) => Err(AccelError::MetalInit(msg.clone())),
        }
    }

    fn init() -> AccelResult<MetalContext> {
        autoreleasepool(|_| {
            let device = MTLCreateSystemDefaultDevice()
                .ok_or_else(|| AccelError::MetalInit("no Metal device found".into()))?;
            // The metallib is `'static`, so Metal can reference it in place.
            let data = DispatchData::from_static_bytes(METALLIB);
            let library = device
                .newLibraryWithData_error(&data)
                .map_err(|e| AccelError::ShaderCompilation(error_description(&e)))?;
            let function = library
                .newFunctionWithName(&NSString::from_str(CLIP_KERNEL))
                .ok_or_else(|| {
                    AccelError::ShaderCompilation(format!(
                        "{CLIP_KERNEL}: Function '{CLIP_KERNEL}' does not exist"
                    ))
                })?;
            let clip_pipeline = device
                .newComputePipelineStateWithFunction_error(&function)
                .map_err(|e| AccelError::ShaderCompilation(error_description(&e)))?;

            let width = clip_pipeline.threadExecutionWidth();
            let max = clip_pipeline.maxTotalThreadsPerThreadgroup();
            let threads_per_group = (width * SIMD_GROUPS_PER_THREADGROUP).min(max).max(1);
            info!(
                device = %device.name(),
                simd_width = width,
                max_threads = max,
                threads_per_group,
                "Metal GPU initialized"
            );

            let command_queue = device
                .newCommandQueue()
                .ok_or_else(|| AccelError::MetalInit("could not create a command queue".into()))?;
            Ok(MetalContext {
                device,
                command_queue,
                clip_pipeline,
                threads_per_group,
            })
        })
    }

    pub(crate) fn device(&self) -> &Device {
        &self.device
    }

    pub(crate) fn command_queue(&self) -> &CommandQueue {
        &self.command_queue
    }

    pub(crate) fn clip_pipeline(&self) -> &ComputePipelineState {
        &self.clip_pipeline
    }

    pub(crate) fn threads_per_group(&self) -> usize {
        self.threads_per_group
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<MetalContext>();
    }
}
