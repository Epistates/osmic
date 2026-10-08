use std::marker::PhantomData;
use std::mem::{align_of, size_of};

use bytemuck::Pod;
use metal::foreign_types::ForeignType;
use metal::{Device, MTLResourceOptions};

use crate::error::{AccelError, AccelResult};

/// Typed Metal buffer in unified (shared) memory.
///
/// On Apple Silicon the CPU and GPU address the same physical memory, so there
/// are no copies. Because bytes are produced by GPU kernels, `T` must be
/// [`Pod`]: every bit pattern is a valid `T`.
///
/// `Send`/`Sync` are derived automatically (`metal::Buffer` is
/// `Send + Sync`, `PhantomData<T>` follows `T`), so no `unsafe impl` is needed.
///
/// # Synchronisation contract
///
/// `as_slice`/`as_mut_slice` are `unsafe`: the caller must guarantee that no
/// command buffer that reads or writes this buffer is in flight. The batch
/// typestate in [`super::batch`] enforces that for all uses in this crate.
pub(crate) struct MetalBuffer<T: Pod> {
    buffer: metal::Buffer,
    len: usize,
    _marker: PhantomData<T>,
}

impl<T: Pod> MetalBuffer<T> {
    /// Create a zero-initialised buffer of `len` elements (`len > 0`).
    pub(crate) fn new(device: &Device, len: usize) -> AccelResult<Self> {
        let byte_len = Self::byte_len(device, len)?;
        let buffer = device.new_buffer(byte_len, MTLResourceOptions::StorageModeShared);
        Self::wrap(buffer, len)
    }

    /// Create a buffer initialised from `data` (non-empty).
    pub(crate) fn from_slice(device: &Device, data: &[T]) -> AccelResult<Self> {
        let byte_len = Self::byte_len(device, data.len())?;
        let bytes: &[u8] = bytemuck::cast_slice(data);
        let buffer = device.new_buffer_with_data(
            bytes.as_ptr().cast(),
            byte_len,
            MTLResourceOptions::StorageModeShared,
        );
        Self::wrap(buffer, data.len())
    }

    fn byte_len(device: &Device, len: usize) -> AccelResult<u64> {
        if len == 0 {
            return Err(AccelError::BufferCreation("zero-length buffer".into()));
        }
        let bytes = len
            .checked_mul(size_of::<T>())
            .map(|b| b as u64)
            .filter(|&b| b <= device.max_buffer_length())
            .ok_or_else(|| {
                AccelError::BufferCreation(format!(
                    "{len} elements of {} bytes exceed the device's maximum buffer length ({} bytes); \
                     split the batch",
                    size_of::<T>(),
                    device.max_buffer_length()
                ))
            })?;
        Ok(bytes)
    }

    fn wrap(buffer: metal::Buffer, len: usize) -> AccelResult<Self> {
        // `newBuffer` returns nil on allocation failure.
        if buffer.as_ptr().is_null() {
            return Err(AccelError::BufferCreation(
                "Metal returned a null buffer (out of memory?)".into(),
            ));
        }
        let contents = buffer.contents();
        if contents.is_null() || !(contents as usize).is_multiple_of(align_of::<T>()) {
            return Err(AccelError::BufferCreation(
                "buffer contents are null or misaligned".into(),
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
        // SAFETY: `contents()` is non-null and `align_of::<T>()`-aligned
        // (checked in `wrap`) and points to `len * size_of::<T>()` bytes of
        // shared memory kept alive by `self.buffer`. `T: Pod` makes every bit
        // pattern valid. The caller guarantees no concurrent GPU access, and
        // `&self` prevents a concurrent CPU mutable borrow.
        unsafe { std::slice::from_raw_parts(self.buffer.contents().cast::<T>(), self.len) }
    }

    /// Mutably view the buffer contents.
    ///
    /// # Safety
    ///
    /// Same as [`Self::as_slice`].
    pub(crate) unsafe fn as_mut_slice(&mut self) -> &mut [T] {
        // SAFETY: as for `as_slice`; `&mut self` guarantees exclusive CPU access.
        unsafe { std::slice::from_raw_parts_mut(self.buffer.contents().cast::<T>(), self.len) }
    }

    /// The underlying Metal buffer, for binding to a compute encoder.
    pub(crate) fn metal_buffer(&self) -> &metal::BufferRef {
        &self.buffer
    }
}
