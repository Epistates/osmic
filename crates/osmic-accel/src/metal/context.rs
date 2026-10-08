use std::sync::{Arc, OnceLock};

use metal::{CommandQueue, ComputePipelineState, Device};
use objc::rc::autoreleasepool;
use tracing::info;

use crate::error::{AccelError, AccelResult};

/// Compiled shader library embedded by `build.rs` (this module only exists
/// when the `osmic_metallib` cfg is set, i.e. the metallib was produced).
static METALLIB: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/osmic_geometry.metallib"));

const CLIP_KERNEL: &str = "clip_units";

/// Threadgroup width as a multiple of the pipeline's SIMD width.
const SIMD_GROUPS_PER_THREADGROUP: u64 = 4;

/// Process-wide Metal state: device, queue and the compiled clip pipeline.
///
/// All fields are `Send + Sync` Metal objects (the `metal` crate marks them
/// so), hence the struct is `Send + Sync` without any `unsafe impl`.
pub(crate) struct MetalContext {
    device: Device,
    command_queue: CommandQueue,
    clip_pipeline: ComputePipelineState,
    threads_per_group: u64,
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
        autoreleasepool(|| {
            let device = Device::system_default()
                .ok_or_else(|| AccelError::MetalInit("no Metal device found".into()))?;
            let library = device
                .new_library_with_data(METALLIB)
                .map_err(AccelError::ShaderCompilation)?;
            let function = library
                .get_function(CLIP_KERNEL, None)
                .map_err(|e| AccelError::ShaderCompilation(format!("{CLIP_KERNEL}: {e}")))?;
            let clip_pipeline = device
                .new_compute_pipeline_state_with_function(&function)
                .map_err(AccelError::ShaderCompilation)?;

            let width = clip_pipeline.thread_execution_width();
            let max = clip_pipeline.max_total_threads_per_threadgroup();
            let threads_per_group = (width * SIMD_GROUPS_PER_THREADGROUP).min(max).max(1);
            info!(
                device = %device.name(),
                simd_width = width,
                max_threads = max,
                threads_per_group,
                "Metal GPU initialized"
            );

            let command_queue = device.new_command_queue();
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

    pub(crate) fn threads_per_group(&self) -> u64 {
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
