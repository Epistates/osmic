//! One uploaded batch and its lifecycle.
//!
//! The lifecycle is a typestate chain that makes reading GPU output before
//! completion impossible in safe code:
//!
//! ```text
//! GpuBatch --dispatch(self)--> InFlight --wait(self)--> CompletedBatch
//! ```
//!
//! Output buffers are only reachable from [`CompletedBatch`], which can only be
//! produced by [`InFlight::wait`] after the command buffer reached the
//! `Completed` state without error.

use std::mem::size_of;
use std::ptr::NonNull;
use std::sync::Arc;
use std::time::Instant;

use objc2::rc::{Retained, autoreleasepool};
use objc2_metal::{
    MTLCommandBuffer, MTLCommandBufferError, MTLCommandBufferStatus, MTLCommandEncoder,
    MTLCommandQueue, MTLComputeCommandEncoder, MTLSize,
};

use crate::clip::{ClipOptions, UnitResults, UnitView};
use crate::error::{AccelError, AccelResult};
use crate::prepare::{Prepared, UnitKind};

use super::buffer::MetalBuffer;
use super::context::MetalContext;
use super::types::*;
use super::{CommandBuffer, error_description};

/// `MTLCommandBufferErrorTimeout`, as the `NSError` code it is reported with.
const NS_ERROR_CODE_TIMEOUT: isize = MTLCommandBufferError::Timeout.0 as isize;

/// A batch whose inputs are uploaded and whose output buffers are allocated,
/// but which has not been dispatched yet.
pub(crate) struct GpuBatch {
    ctx: Arc<MetalContext>,
    coords: MetalBuffer<[f32; 2]>,
    out_points: MetalBuffer<[f32; 2]>,
    scratch: MetalBuffer<[f32; 2]>,
    part_lens: MetalBuffer<u32>,
    unit_buffer: MetalBuffer<GpuUnit>,
    results: MetalBuffer<GpuUnitResult>,
    /// Host copy of the unit descriptors (offsets/capacities for readback).
    units: Vec<GpuUnit>,
}

fn to_u32(value: u64, what: &str) -> AccelResult<u32> {
    u32::try_from(value).map_err(|_| {
        AccelError::BufferCreation(format!(
            "{what} ({value}) exceeds 32-bit GPU indexing; split the batch"
        ))
    })
}

impl GpuBatch {
    /// Upload `prepared` and allocate outputs. Returns `None` when the batch
    /// has no units (e.g. only points), in which case there is nothing to run.
    pub(crate) fn upload(
        ctx: &Arc<MetalContext>,
        prepared: &Prepared,
        options: &ClipOptions,
    ) -> AccelResult<Option<Self>> {
        if prepared.units.is_empty() {
            return Ok(None);
        }

        // Lay out per-unit output regions.
        let mut out_total = 0u64;
        let mut scratch_total = 0u64;
        let mut parts_total = 0u64;
        let mut units = Vec::with_capacity(prepared.units.len());
        for unit in &prepared.units {
            let (kind, out_cap, scratch_cap, part_cap) = match unit.kind {
                UnitKind::Ring => {
                    let cap = options.ring_capacity(unit.len);
                    (KIND_RING, cap, cap, 0)
                }
                // At most two points per segment and one part per segment.
                UnitKind::Line => (
                    KIND_LINE,
                    2 * u64::from(unit.len - 1),
                    0,
                    u64::from(unit.len - 1),
                ),
            };
            units.push(GpuUnit {
                coord_offset: unit.start,
                coord_count: unit.len,
                kind,
                out_offset: to_u32(out_total, "output vertices")?,
                out_capacity: to_u32(out_cap, "unit output capacity")?,
                scratch_offset: to_u32(scratch_total, "scratch vertices")?,
                part_offset: to_u32(parts_total, "line parts")?,
                part_capacity: to_u32(part_cap, "unit part capacity")?,
                min_x: unit.bounds.min_x,
                min_y: unit.bounds.min_y,
                max_x: unit.bounds.max_x,
                max_y: unit.bounds.max_y,
            });
            out_total += out_cap;
            scratch_total += scratch_cap;
            parts_total += part_cap;
        }
        to_u32(out_total, "output vertices")?;
        to_u32(scratch_total, "scratch vertices")?;
        to_u32(parts_total, "line parts")?;

        autoreleasepool(|_| {
            let device = ctx.device();
            let coords = MetalBuffer::from_slice(device, &prepared.coords)?;
            let out_points = MetalBuffer::new(device, out_total.max(1) as usize)?;
            let scratch = MetalBuffer::new(device, scratch_total.max(1) as usize)?;
            let part_lens = MetalBuffer::new(device, parts_total.max(1) as usize)?;
            let unit_buffer = MetalBuffer::from_slice(device, &units)?;
            let mut results = MetalBuffer::<GpuUnitResult>::new(device, units.len())?;

            // SAFETY: the buffer was just created and has not been bound to
            // any command buffer, so the GPU cannot be accessing it.
            let init = unsafe { results.as_mut_slice() };
            init.fill(GpuUnitResult {
                out_count: 0,
                part_count: 0,
                status: STATUS_UNPROCESSED,
                _pad: 0,
            });

            Ok(Some(GpuBatch {
                ctx: Arc::clone(ctx),
                coords,
                out_points,
                scratch,
                part_lens,
                unit_buffer,
                results,
                units,
            }))
        })
    }

    /// Encode and commit the clip kernel, consuming the batch.
    pub(crate) fn dispatch(self) -> AccelResult<InFlight> {
        let unit_count = u32::try_from(self.units.len())
            .map_err(|_| AccelError::ExecutionFailed("too many units".into()))?;
        let params = GpuClipParams {
            unit_count,
            _pad0: 0,
            _pad1: 0,
            _pad2: 0,
        };

        let command_buffer = autoreleasepool(|_| {
            let ctx = &self.ctx;
            let command_buffer = ctx.command_queue().commandBuffer().ok_or_else(|| {
                AccelError::ExecutionFailed("could not create a command buffer".into())
            })?;
            let encoder = command_buffer.computeCommandEncoder().ok_or_else(|| {
                AccelError::ExecutionFailed("could not create a compute command encoder".into())
            })?;
            encoder.setComputePipelineState(ctx.clip_pipeline());
            // SAFETY: indices 0..=6 and the element types bound to them match
            // the `[[buffer(n)]]` parameters of `clip_units` (layouts asserted
            // in `types.rs`), and offset 0 is in bounds of every buffer. The
            // command buffer retains the bound buffers until it completes, and
            // the CPU cannot touch them before then: they move into `InFlight`
            // and are only readable from `CompletedBatch`. `setBytes` copies
            // `size_of::<GpuClipParams>()` bytes from the live `params` local
            // during the call.
            unsafe {
                encoder.setBuffer_offset_atIndex(Some(self.coords.metal_buffer()), 0, 0);
                encoder.setBuffer_offset_atIndex(Some(self.out_points.metal_buffer()), 0, 1);
                encoder.setBuffer_offset_atIndex(Some(self.scratch.metal_buffer()), 0, 2);
                encoder.setBuffer_offset_atIndex(Some(self.part_lens.metal_buffer()), 0, 3);
                encoder.setBuffer_offset_atIndex(Some(self.unit_buffer.metal_buffer()), 0, 4);
                encoder.setBuffer_offset_atIndex(Some(self.results.metal_buffer()), 0, 5);
                encoder.setBytes_length_atIndex(
                    NonNull::from(&params).cast(),
                    size_of::<GpuClipParams>(),
                    6,
                );
            }

            let units = self.units.len();
            let grid = MTLSize {
                width: units,
                height: 1,
                depth: 1,
            };
            let group = MTLSize {
                width: ctx.threads_per_group().min(units),
                height: 1,
                depth: 1,
            };
            encoder.dispatchThreads_threadsPerThreadgroup(grid, group);
            encoder.endEncoding();
            command_buffer.commit();
            // `Retained` holds its own reference, so the command buffer
            // outlives the pool.
            Ok::<_, AccelError>(Committed(command_buffer))
        })?;

        Ok(InFlight {
            batch: self,
            command_buffer,
            started: Instant::now(),
        })
    }
}

/// A command buffer that has been committed: no more commands are encoded
/// into it, it is only waited on and queried.
struct Committed(Retained<CommandBuffer>);

// SAFETY: `objc2-metal` leaves `MTLCommandBuffer` `!Send`/`!Sync` because
// *encoding* into a command buffer is single-threaded. A `Committed` is only
// built after `commit`, so encoding is over, and the only messages it is ever
// sent are `waitUntilCompleted`, `status` and `error`. Metal itself updates the
// status and error from its own completion thread, so those are designed for
// cross-thread use, and retain/release is thread-safe for every Objective-C
// object. (wgpu-hal makes its Metal command buffers `Send + Sync` likewise.)
unsafe impl Send for Committed {}
// SAFETY: see the `Send` impl above; all three messages are read-only queries
// or blocking waits that are safe to issue concurrently.
unsafe impl Sync for Committed {}

/// A committed batch. Its buffers are inaccessible until [`InFlight::wait`].
pub(crate) struct InFlight {
    batch: GpuBatch,
    command_buffer: Committed,
    started: Instant,
}

impl InFlight {
    /// Block until the GPU finishes, then verify it finished successfully and
    /// that every unit reported a usable status.
    pub(crate) fn wait(self) -> AccelResult<CompletedBatch> {
        let InFlight {
            batch,
            command_buffer,
            started,
        } = self;

        let Committed(command_buffer) = &command_buffer;
        autoreleasepool(|_| {
            command_buffer.waitUntilCompleted();
            match command_buffer.status() {
                MTLCommandBufferStatus::Completed => Ok(()),
                MTLCommandBufferStatus::Error => {
                    let (code, description) = command_buffer_error(command_buffer);
                    if code == NS_ERROR_CODE_TIMEOUT {
                        Err(AccelError::GpuTimeout(started.elapsed()))
                    } else {
                        Err(AccelError::ExecutionFailed(format!(
                            "{description} (MTLCommandBufferError {code})"
                        )))
                    }
                }
                other => Err(AccelError::ExecutionFailed(format!(
                    "command buffer finished in unexpected state {other:?}"
                ))),
            }
        })?;

        // SAFETY: the command buffer reached `Completed`, so the GPU no
        // longer touches `results`.
        let results = unsafe { batch.results.as_slice() }.to_vec();
        debug_assert_eq!(results.len(), batch.units.len());
        for (index, (unit, result)) in batch.units.iter().zip(&results).enumerate() {
            match result.status {
                STATUS_OK => {
                    if result.out_count > unit.out_capacity
                        || result.part_count > unit.part_capacity
                    {
                        return Err(AccelError::ExecutionFailed(format!(
                            "unit {index}: kernel reported {} vertices / {} parts beyond capacity {} / {}",
                            result.out_count,
                            result.part_count,
                            unit.out_capacity,
                            unit.part_capacity
                        )));
                    }
                }
                STATUS_OVERFLOW => {}
                STATUS_UNPROCESSED => {
                    return Err(AccelError::ExecutionFailed(format!(
                        "unit {index} was never processed by the kernel"
                    )));
                }
                STATUS_INVALID => {
                    return Err(AccelError::ExecutionFailed(format!(
                        "unit {index}: kernel rejected the unit descriptor"
                    )));
                }
                other => {
                    return Err(AccelError::ExecutionFailed(format!(
                        "unit {index}: unknown kernel status {other}"
                    )));
                }
            }
        }

        Ok(CompletedBatch { batch, results })
    }
}

/// `(code, localizedDescription)` of the command buffer's `NSError`.
fn command_buffer_error(command_buffer: &CommandBuffer) -> (isize, String) {
    match command_buffer.error() {
        Some(error) => (error.code(), error_description(&error)),
        None => (0, "unknown GPU error".into()),
    }
}

/// A batch the GPU has finished with; the only type that exposes output.
pub(crate) struct CompletedBatch {
    batch: GpuBatch,
    results: Vec<GpuUnitResult>,
}

impl CompletedBatch {
    /// Did this unit exceed its capacity (its output is absent and the item
    /// must be recomputed elsewhere)?
    pub(crate) fn overflowed(&self, unit: usize) -> bool {
        self.results[unit].status == STATUS_OVERFLOW
    }
}

impl UnitResults for CompletedBatch {
    fn unit(&self, index: usize) -> UnitView<'_> {
        let unit = &self.batch.units[index];
        let result = &self.results[index];
        if result.status != STATUS_OK {
            return UnitView {
                points: &[],
                parts: &[],
            };
        }
        // SAFETY: a `CompletedBatch` only exists after `InFlight::wait`
        // observed the command buffer in the `Completed` state.
        let (points, parts) = unsafe {
            (
                self.batch.out_points.as_slice(),
                self.batch.part_lens.as_slice(),
            )
        };
        let p0 = unit.out_offset as usize;
        let q0 = unit.part_offset as usize;
        UnitView {
            // Ranges were validated against capacities in `wait`, and the
            // capacities were laid out inside the buffers in `upload`.
            points: &points[p0..p0 + result.out_count as usize],
            parts: &parts[q0..q0 + result.part_count as usize],
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::cpu;
    use crate::prepare::Bounds;

    use super::*;

    /// The shared context, or `None` (skip) on a machine without Metal.
    fn context() -> Option<Arc<MetalContext>> {
        match MetalContext::get() {
            Ok(ctx) => Some(ctx),
            Err(AccelError::NotAvailable) => None,
            Err(e) => panic!("Metal initialisation failed: {e}"),
        }
    }

    fn square_bounds() -> Bounds {
        Bounds {
            min_x: 0.0,
            min_y: 0.0,
            max_x: 10.0,
            max_y: 10.0,
        }
    }

    fn prepared_ring(ring: &[[f32; 2]]) -> Prepared {
        use crate::prepare::{Plan, Unit};
        Prepared {
            coords: ring.to_vec(),
            units: vec![Unit {
                kind: UnitKind::Ring,
                start: 0,
                len: ring.len() as u32,
                bounds: square_bounds(),
            }],
            plans: vec![Plan::Polygons {
                first_unit: 0,
                ring_counts: vec![1],
            }],
        }
    }

    /// Run a prepared batch on the GPU, returning `None` if there is no GPU.
    fn run(prepared: &Prepared, options: &ClipOptions) -> Option<CompletedBatch> {
        let ctx = context()?;
        let batch = GpuBatch::upload(&ctx, prepared, options).unwrap().unwrap();
        Some(batch.dispatch().unwrap().wait().unwrap())
    }

    #[test]
    fn overflow_status_matches_cpu_capacity_semantics() {
        // A triangle poking out the left and right edges grows in each stage.
        let tri = [[-5.0, 5.0], [15.0, 2.0], [15.0, 8.0]];
        let prepared = prepared_ring(&tri);

        // Capacity exactly the input size: stage output must exceed it.
        let tight = ClipOptions {
            ring_capacity_factor: 1,
            ring_capacity_slack: 0,
            ..ClipOptions::default()
        };
        let Some(done) = run(&prepared, &tight) else {
            return;
        };
        let (mut out, mut scratch) = (Vec::new(), Vec::new());
        let cpu_overflow = cpu::clip_ring(&tri, &square_bounds(), 3, &mut out, &mut scratch);
        assert!(cpu_overflow.is_err());
        assert!(done.overflowed(0));

        // Generous capacity: no overflow, and the result matches the CPU.
        let roomy = ClipOptions::default();
        let done = run(&prepared, &roomy).unwrap();
        assert!(!done.overflowed(0));
        cpu::clip_ring(&tri, &square_bounds(), usize::MAX, &mut out, &mut scratch).unwrap();
        let gpu = done.unit(0).points;
        assert_eq!(gpu.len(), out.len());
        for (g, c) in gpu.iter().zip(&out) {
            assert!((g[0] - c[0]).abs() < 1e-3 && (g[1] - c[1]).abs() < 1e-3);
        }
    }

    #[test]
    fn unprocessed_marker_is_not_a_valid_status() {
        assert_ne!(STATUS_UNPROCESSED, STATUS_OK);
        assert_ne!(STATUS_UNPROCESSED, STATUS_OVERFLOW);
    }
}
