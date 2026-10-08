use std::ffi::c_void;
use std::marker::PhantomData;
use std::mem::size_of;
use std::ptr::NonNull;

use bytemuck::Pod;
use objc2::rc::Retained;
use objc2_metal::{MTLBuffer, MTLDevice, MTLResourceOptions};

use crate::error::{AccelError, AccelResult};

use super::{Buffer, Device};

/// Typed Metal buffer in unified (shared) memory.
///
/// On Apple Silicon the CPU and GPU address the same physical memory, so there
/// are no copies. Because bytes are produced by GPU kernels, `T` must be
/// [`Pod`]: every bit pattern is a valid `T`.
///
/// # Synchronisation contract
///
/// `as_slice`/`as_mut_slice` are `unsafe`: the caller must guarantee that no
/// command buffer that reads or writes this buffer is in flight. The batch
/// typestate in [`super::batch`] enforces that for all uses in this crate.
pub(crate) struct MetalBuffer<T: Pod> {
    buffer: Retained<Buffer>,
    len: usize,
    _marker: PhantomData<T>,
}

// SAFETY: `objc2-metal` leaves `MTLBuffer` `!Send`/`!Sync` because `contents`
// exposes memory whose synchronisation it cannot check, not because the object
// itself is thread-bound: retaining/releasing a Metal resource and reading its
// immutable properties (`contents`, `length`) is safe from any thread (wgpu-hal
// relies on the same guarantee for its Metal buffers). Access to the contents
// goes only through `as_slice`/`as_mut_slice`, whose contract excludes
// concurrent GPU access, while `&`/`&mut` exclude CPU data races. The
// `T: Send`/`T: Sync` bounds are what the auto traits would require of `T`.
unsafe impl<T: Pod + Send> Send for MetalBuffer<T> {}
// SAFETY: see the `Send` impl above; `&MetalBuffer` only permits shared reads.
unsafe impl<T: Pod + Sync> Sync for MetalBuffer<T> {}

impl<T: Pod> MetalBuffer<T> {
    /// Create a zero-initialised buffer of `len` elements (`len > 0`).
    pub(crate) fn new(device: &Device, len: usize) -> AccelResult<Self> {
        let byte_len = Self::byte_len(device, len)?;
        let buffer =
            device.newBufferWithLength_options(byte_len, MTLResourceOptions::StorageModeShared);
        Self::wrap(buffer, len)
    }

    /// Create a buffer initialised from `data` (non-empty).
    pub(crate) fn from_slice(device: &Device, data: &[T]) -> AccelResult<Self> {
        let byte_len = Self::byte_len(device, data.len())?;
        let bytes: &[u8] = bytemuck::cast_slice(data);
        // SAFETY: `bytes` is a live, non-empty (checked by `byte_len`) slice of
        // exactly `byte_len` readable bytes, borrowed for the whole call; Metal
        // copies them into the new buffer before returning.
        let buffer = unsafe {
            device.newBufferWithBytes_length_options(
                NonNull::from(bytes).cast::<c_void>(),
                byte_len,
                MTLResourceOptions::StorageModeShared,
            )
        };
        Self::wrap(buffer, data.len())
    }

    fn byte_len(device: &Device, len: usize) -> AccelResult<usize> {
        if len == 0 {
            return Err(AccelError::BufferCreation("zero-length buffer".into()));
        }
        let bytes = len
            .checked_mul(size_of::<T>())
            .filter(|&b| b <= device.maxBufferLength())
            .ok_or_else(|| {
                AccelError::BufferCreation(format!(
                    "{len} elements of {} bytes exceed the device's maximum buffer length ({} bytes); \
                     split the batch",
                    size_of::<T>(),
                    device.maxBufferLength()
                ))
            })?;
        Ok(bytes)
    }

    fn wrap(buffer: Option<Retained<Buffer>>, len: usize) -> AccelResult<Self> {
        // `newBuffer` returns nil on allocation failure.
        let buffer = buffer.ok_or_else(|| {
            AccelError::BufferCreation("Metal returned a null buffer (out of memory?)".into())
        })?;
        if !buffer.contents().cast::<T>().is_aligned() {
            return Err(AccelError::BufferCreation(
                "buffer contents are misaligned".into(),
            ));
        }
        Ok(Self {
            buffer,
            len,
            _marker: PhantomData,
        })
    }

    /// View the buffer contents.
    ///
    /// # Safety
    ///
    /// No GPU command buffer using this buffer may be in flight (the GPU may
    /// otherwise be writing the memory, which would be a data race).
    pub(crate) unsafe fn as_slice(&self) -> &[T] {
        // SAFETY: `contents()` is non-null (`NonNull`) and
        // `align_of::<T>()`-aligned (checked in `wrap`) and points to
        // `len * size_of::<T>()` bytes of shared memory kept alive by
        // `self.buffer`. `T: Pod` makes every bit pattern valid. The caller
        // guarantees no concurrent GPU access, and `&self` prevents a
        // concurrent CPU mutable borrow.
        unsafe { std::slice::from_raw_parts(self.contents(), self.len) }
    }

    /// Mutably view the buffer contents.
    ///
    /// # Safety
    ///
    /// Same as [`Self::as_slice`].
    pub(crate) unsafe fn as_mut_slice(&mut self) -> &mut [T] {
        // SAFETY: as for `as_slice`; `&mut self` guarantees exclusive CPU access.
        unsafe { std::slice::from_raw_parts_mut(self.contents(), self.len) }
    }

    fn contents(&self) -> *mut T {
        self.buffer.contents().cast::<T>().as_ptr()
    }

    /// The underlying Metal buffer, for binding to a compute encoder.
    pub(crate) fn metal_buffer(&self) -> &Buffer {
        &self.buffer
    }
}
