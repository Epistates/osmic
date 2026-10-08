//! Public entry points: [`GpuAccelerator`], [`PendingBatch`] and the
//! backend-agnostic [`Clipper`].

use crate::clip::{clip_batch_cpu, ClipOptions, ClippedGeometry, WorkItem};
use crate::error::{AccelError, AccelResult};

#[cfg(osmic_metallib)]
use crate::metal::accelerator as backend;

/// Stand-in backend when no metallib was embedded (non-macOS, or the Metal
/// toolchain was missing at build time). It cannot be constructed, so none of
/// its methods can run; they exist so the public API is identical everywhere.
#[cfg(not(osmic_metallib))]
mod backend {
    use std::convert::Infallible;
    use std::marker::PhantomData;

    use crate::clip::{ClipOptions, ClippedGeometry, WorkItem};
    use crate::error::{AccelError, AccelResult};

    pub(crate) struct Accelerator(Infallible);

    pub(crate) struct Pending<'a>(Infallible, PhantomData<&'a ()>);

    impl Accelerator {
        pub(crate) fn is_available() -> bool {
            false
        }

        pub(crate) fn new(_options: ClipOptions) -> AccelResult<Self> {
            Err(AccelError::NotAvailable)
        }

        pub(crate) fn clip_async<'a>(
            &self,
            _items: &'a [WorkItem<'a>],
        ) -> AccelResult<Pending<'a>> {
            match self.0 {}
        }
    }

    impl Pending<'_> {
        pub(crate) fn wait(self) -> AccelResult<Vec<ClippedGeometry>> {
            match self.0 {}
        }
    }
}

/// Whether the Metal backend can be used in this process: the crate was built
/// with the shader library on an Apple platform and a Metal device exists.
pub fn is_available() -> bool {
    backend::Accelerator::is_available()
}

/// GPU clipper (Apple Silicon Metal).
///
/// Obtain one with [`GpuAccelerator::new`]; it fails with
/// [`AccelError::NotAvailable`] when [`is_available`] is false. The value is
/// cheap to share (`Send + Sync`) and may be used from many threads.
///
/// Coordinates are projected on the CPU in f64, clipped on the GPU in f32.
/// Items whose rings exceed GPU scratch capacity are transparently recomputed
/// on the CPU, so results are always complete (see [`ClipOptions`]).
pub struct GpuAccelerator {
    inner: backend::Accelerator,
}

impl GpuAccelerator {
    /// Create an accelerator with default [`ClipOptions`].
    pub fn new() -> AccelResult<Self> {
        Self::with_options(ClipOptions::default())
    }

    /// Create an accelerator with explicit options.
    pub fn with_options(options: ClipOptions) -> AccelResult<Self> {
        Ok(Self {
            inner: backend::Accelerator::new(options)?,
        })
    }

    /// Clip a batch and wait for the results (in input order).
    pub fn clip_batch(&self, items: &[WorkItem<'_>]) -> AccelResult<Vec<ClippedGeometry>> {
        self.clip_batch_async(items)?.wait()
    }

    /// Start clipping a batch and return immediately with a [`PendingBatch`].
    ///
    /// Inputs are projected and uploaded before this returns; only the GPU
    /// execution overlaps with the caller's work.
    pub fn clip_batch_async<'a>(&self, items: &'a [WorkItem<'a>]) -> AccelResult<PendingBatch<'a>> {
        Ok(PendingBatch {
            inner: self.inner.clip_async(items)?,
        })
    }
}

/// A batch submitted to the GPU. The only way to obtain its results is
/// [`PendingBatch::wait`], which blocks until the GPU finished and verified
/// the run; there is no way to observe partially written output.
///
/// Dropping a `PendingBatch` without waiting is safe: Metal keeps the buffers
/// alive until the command buffer completes.
#[must_use = "results are only available through `wait`"]
pub struct PendingBatch<'a> {
    inner: backend::Pending<'a>,
}

impl PendingBatch<'_> {
    /// Block until the GPU completes and return results in input order.
    ///
    /// Errors: [`AccelError::GpuTimeout`] if the GPU watchdog aborted the run,
    /// [`AccelError::ExecutionFailed`] for any other command-buffer error or
    /// invalid kernel output.
    pub fn wait(self) -> AccelResult<Vec<ClippedGeometry>> {
        self.inner.wait()
    }
}

/// Which implementation a [`Clipper`] runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Backend {
    /// Metal GPU.
    Gpu,
    /// Portable CPU reference.
    Cpu,
}

/// Single entry point that uses the GPU when available and the CPU otherwise.
///
/// The backend is chosen once at construction. Runtime GPU failures are
/// returned as errors rather than silently retried on the CPU, so faults stay
/// visible.
pub struct Clipper {
    gpu: Option<GpuAccelerator>,
    options: ClipOptions,
}

impl Clipper {
    /// GPU if available, else CPU, with default options.
    pub fn new() -> Self {
        Self::auto(ClipOptions::default())
    }

    /// GPU if available, else CPU.
    pub fn with_options(options: ClipOptions) -> AccelResult<Self> {
        options.validate()?;
        Ok(Self::auto(options))
    }

    /// Always the CPU reference path.
    pub fn cpu(options: ClipOptions) -> AccelResult<Self> {
        options.validate()?;
        Ok(Self { gpu: None, options })
    }

    fn auto(options: ClipOptions) -> Self {
        let gpu = match GpuAccelerator::with_options(options.clone()) {
            Ok(gpu) => Some(gpu),
            Err(AccelError::NotAvailable) => None,
            Err(error) => {
                tracing::warn!(%error, "GPU backend failed to initialise; using the CPU path");
                None
            }
        };
        Self { gpu, options }
    }

    /// The backend in use.
    pub fn backend(&self) -> Backend {
        if self.gpu.is_some() {
            Backend::Gpu
        } else {
            Backend::Cpu
        }
    }

    /// Clip a batch (results in input order).
    pub fn clip_batch(&self, items: &[WorkItem<'_>]) -> AccelResult<Vec<ClippedGeometry>> {
        match &self.gpu {
            Some(gpu) => gpu.clip_batch(items),
            None => clip_batch_cpu(items, &self.options),
        }
    }
}

impl Default for Clipper {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_types_are_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<GpuAccelerator>();
        assert_send_sync::<Clipper>();
        assert_send_sync::<ClipOptions>();
        assert_send_sync::<ClippedGeometry>();
    }

    #[cfg(not(osmic_metallib))]
    mod without_metallib {
        use super::*;

        #[test]
        fn gpu_is_reported_unavailable() {
            assert!(!is_available());
            assert!(matches!(
                GpuAccelerator::new(),
                Err(AccelError::NotAvailable)
            ));
        }

        #[test]
        fn clipper_falls_back_to_cpu() {
            assert_eq!(Clipper::new().backend(), Backend::Cpu);
            assert_eq!(Clipper::default().clip_batch(&[]).unwrap(), vec![]);
        }
    }
}
