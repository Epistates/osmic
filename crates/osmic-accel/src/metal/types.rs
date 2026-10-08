//! Structs shared with `src/kernels/geometry_ops.metal`.
//!
//! Every struct is `#[repr(C)]`, composed solely of 4-byte scalars (so no
//! implicit padding exists and `bytemuck::Pod` can be derived safely), and its
//! size, alignment and field offsets are asserted at compile time against the
//! layout documented in the shader.

use std::mem::{align_of, offset_of, size_of};

use bytemuck::{Pod, Zeroable};

/// Unit kinds (`KIND_*` in the shader).
pub(crate) const KIND_LINE: u32 = 1;
pub(crate) const KIND_RING: u32 = 2;

/// Result status codes (`STATUS_*` in the shader).
pub(crate) const STATUS_OK: u32 = 0;
pub(crate) const STATUS_OVERFLOW: u32 = 1;
pub(crate) const STATUS_INVALID: u32 = 2;
/// Host-initialised marker: the kernel never wrote this unit's result.
pub(crate) const STATUS_UNPROCESSED: u32 = u32::MAX;

/// Per-unit descriptor (`GpuUnit` in the shader).
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub(crate) struct GpuUnit {
    pub coord_offset: u32,
    pub coord_count: u32,
    pub kind: u32,
    pub out_offset: u32,
    pub out_capacity: u32,
    pub scratch_offset: u32,
    pub part_offset: u32,
    pub part_capacity: u32,
    pub min_x: f32,
    pub min_y: f32,
    pub max_x: f32,
    pub max_y: f32,
}

/// Per-unit result written by the kernel (`GpuUnitResult` in the shader).
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub(crate) struct GpuUnitResult {
    pub out_count: u32,
    pub part_count: u32,
    pub status: u32,
    pub _pad: u32,
}

/// Kernel parameters (`GpuClipParams` in the shader), bound with `set_bytes`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub(crate) struct GpuClipParams {
    pub unit_count: u32,
    pub _pad0: u32,
    pub _pad1: u32,
    pub _pad2: u32,
}

const _: () = {
    assert!(size_of::<GpuUnit>() == 48);
    assert!(align_of::<GpuUnit>() == 4);
    assert!(offset_of!(GpuUnit, coord_offset) == 0);
    assert!(offset_of!(GpuUnit, coord_count) == 4);
    assert!(offset_of!(GpuUnit, kind) == 8);
    assert!(offset_of!(GpuUnit, out_offset) == 12);
    assert!(offset_of!(GpuUnit, out_capacity) == 16);
    assert!(offset_of!(GpuUnit, scratch_offset) == 20);
    assert!(offset_of!(GpuUnit, part_offset) == 24);
    assert!(offset_of!(GpuUnit, part_capacity) == 28);
    assert!(offset_of!(GpuUnit, min_x) == 32);
    assert!(offset_of!(GpuUnit, min_y) == 36);
    assert!(offset_of!(GpuUnit, max_x) == 40);
    assert!(offset_of!(GpuUnit, max_y) == 44);

    assert!(size_of::<GpuUnitResult>() == 16);
    assert!(align_of::<GpuUnitResult>() == 4);
    assert!(offset_of!(GpuUnitResult, out_count) == 0);
    assert!(offset_of!(GpuUnitResult, part_count) == 4);
    assert!(offset_of!(GpuUnitResult, status) == 8);
    assert!(offset_of!(GpuUnitResult, _pad) == 12);

    assert!(size_of::<GpuClipParams>() == 16);
    assert!(align_of::<GpuClipParams>() == 4);
    assert!(offset_of!(GpuClipParams, unit_count) == 0);
    assert!(offset_of!(GpuClipParams, _pad0) == 4);
    assert!(offset_of!(GpuClipParams, _pad1) == 8);
    assert!(offset_of!(GpuClipParams, _pad2) == 12);

    // MSL `float2` is 8 bytes; the host uses `[f32; 2]` for vertex buffers.
    assert!(size_of::<[f32; 2]>() == 8);
};
