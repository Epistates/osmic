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

/// Why the shared context could not be created.
///
/// The outcome of initialisation is cached for the process lifetime, so the
/// error must be `Clone`; it keeps its kind so every caller sees the same
/// [`AccelError`] variant the first one did.
#[derive(Debug, Clone, PartialEq, Eq)]
enum InitError {
    /// No Metal device exists (for example a VM or CI runner without a GPU).
    NoDevice,
    /// The embedded shader library or its kernel could not be loaded.
    Shader(String),
    /// Any other Metal setup failure.
    Metal(String),
}

impl From<InitError> for AccelError {
    fn from(error: InitError) -> Self {
        match error {
            InitError::NoDevice => AccelError::NotAvailable,
            InitError::Shader(message) => AccelError::ShaderCompilation(message),
            InitError::Metal(message) => AccelError::MetalInit(message),
        }
    }
}

static CONTEXT: OnceLock<Result<Arc<MetalContext>, InitError>> = OnceLock::new();

impl MetalContext {
    /// Shared context, initialised on first use.
    ///
    /// Errors: [`AccelError::NotAvailable`] when the machine has no Metal
    /// device, [`AccelError::ShaderCompilation`] when the embedded shader
    /// library is unusable, [`AccelError::MetalInit`] otherwise.
    pub(crate) fn get() -> AccelResult<Arc<MetalContext>> {
        match CONTEXT.get_or_init(|| Self::init().map(Arc::new)) {
            Ok(ctx) => Ok(Arc::clone(ctx)),
            Err(error) => Err(error.clone().into()),
        }
    }

    fn init() -> Result<MetalContext, InitError> {
        autoreleasepool(|_| {
            let device = MTLCreateSystemDefaultDevice().ok_or(InitError::NoDevice)?;
            // The metallib is `'static`, so Metal can reference it in place.
            let data = DispatchData::from_static_bytes(METALLIB);
            let library = device
                .newLibraryWithData_error(&data)
                .map_err(|e| InitError::Shader(error_description(&e)))?;
            let function = library
                .newFunctionWithName(&NSString::from_str(CLIP_KERNEL))
                .ok_or_else(|| {
                    InitError::Shader(format!(
                        "{CLIP_KERNEL}: Function '{CLIP_KERNEL}' does not exist"
                    ))
                })?;
            let clip_pipeline = device
                .newComputePipelineStateWithFunction_error(&function)
                .map_err(|e| InitError::Shader(error_description(&e)))?;

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
                .ok_or_else(|| InitError::Metal("could not create a command queue".into()))?;
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
    fn init_errors_keep_their_kind() {
        assert!(matches!(
            AccelError::from(InitError::NoDevice),
            AccelError::NotAvailable
        ));
        assert!(matches!(
            AccelError::from(InitError::Shader("bad".into())),
            AccelError::ShaderCompilation(m) if m == "bad"
        ));
        assert!(matches!(
            AccelError::from(InitError::Metal("queue".into())),
            AccelError::MetalInit(m) if m == "queue"
        ));
    }

    /// On a machine with a Metal device the embedded library must load: a
    /// broken metallib is a failure here, never a silent skip.
    #[test]
    fn the_shader_library_loads_wherever_a_device_exists() {
        match MetalContext::get() {
            Ok(_) | Err(AccelError::NotAvailable) => {}
            Err(e) => panic!("Metal initialisation failed: {e}"),
        }
    }

    #[test]
    fn context_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<MetalContext>();
    }
}
