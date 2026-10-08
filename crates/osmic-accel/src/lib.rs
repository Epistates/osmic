//! Geometry clipping for tile generation, with an optional Apple-Silicon
//! Metal GPU backend and a portable CPU reference implementation.
//!
//! Given lon/lat geometries and the tile each should land in, the crate
//! projects them to tile-local coordinates and clips them to the tile (plus a
//! small buffer):
//!
//! - polygons and multipolygons: Sutherland-Hodgman per ring, holes preserved;
//! - lines: Liang-Barsky per segment, producing one output part for every
//!   contiguous run inside the tile (no false connecting segments);
//! - points: containment test.
//!
//! # Opt-in and availability
//!
//! This crate is not used by the default tile pipeline; depend on it only if
//! you want to experiment with GPU offload. The GPU backend is available when
//! **all** of the following hold:
//!
//! 1. the target is macOS (Apple Silicon is the supported and tested
//!    configuration; the shaders use Metal on any GPU Metal supports);
//! 2. the Metal compiler was present at build time (`xcrun metal`, i.e. Xcode,
//!    or the separately downloaded Metal Toolchain on Xcode 26+). `build.rs`
//!    then sets `cfg(osmic_metallib)`. Without it the crate still compiles and
//!    everything works on the CPU; the build prints a warning explaining why;
//! 3. a Metal device exists at runtime.
//!
//! Use [`is_available`] to probe, [`GpuAccelerator::new`] for the GPU alone
//! (it returns [`AccelError::NotAvailable`] otherwise), [`clip_batch_cpu`] for
//! the CPU alone, or [`Clipper`] to pick automatically.
//!
//! On non-macOS targets, on docs.rs (`DOCS_RS` set), or with
//! `OSMIC_ACCEL_SKIP_SHADERS=1`, shaders are not compiled and the GPU is
//! reported as unavailable. `MACOSX_DEPLOYMENT_TARGET` selects the minimum OS
//! the metallib is built for (default 11.0).
//!
//! # Precision and parity
//!
//! The GPU works in `f32` and the shaders are compiled with `-ffast-math`
//! (fused multiply-add, reassociation). Vertex selection decisions use the
//! same f32 inputs and comparisons as the CPU path, but intersection points
//! can differ by a few ulps. GPU and CPU results are therefore
//! **tolerance-equal, not bit-identical**; with tile extents of 4096 expect
//! differences well below `1e-2` tile units. Both are far below the integer
//! quantisation of vector tiles. Inputs must be finite (checked on the host;
//! `-ffast-math` makes NaN/inf behaviour undefined on the GPU).
//!
//! # Performance
//!
//! Projection (f64 Web Mercator) stays on the CPU because Metal has no f64, so
//! end-to-end time is dominated by it: the GPU kernel itself takes
//! 0.4-5 ms for the batches in `examples/bench_clip.rs`, but a batch is only
//! about as fast as the CPU path (0.7x-1.2x measured), and small batches
//! (hundreds of items) are slower on the GPU because of fixed dispatch cost.
//! Benchmark your workload (`cargo run --release --example bench_clip`)
//! before preferring the GPU; [`Clipper::cpu`] forces the CPU path.
//!
//! # Capacity and fallback
//!
//! GPU rings use per-ring scratch/output regions sized by [`ClipOptions`].
//! There is no fixed vertex limit. If an intermediate Sutherland-Hodgman stage
//! would exceed a ring's capacity the kernel flags the item and the host
//! recomputes **that item** on the CPU; output is never truncated.
//!
//! # Semantics notes
//!
//! Sutherland-Hodgman clipping of concave rings can yield coincident edges
//! along the clip boundary where the polygon splits into several pieces; they
//! are degenerate (zero area) and harmless for filling, and the CPU reference
//! behaves identically.
//!
//! # Example
//!
//! ```
//! use geo_types::polygon;
//! use osmic_accel::{Clipper, WorkItem};
//! use osmic_core::geometry::Geometry;
//!
//! let poly = Geometry::Polygon(polygon![
//!     (x: -1.0, y: -1.0), (x: 1.0, y: -1.0), (x: 1.0, y: 1.0), (x: -1.0, y: 1.0),
//! ]);
//! let item = WorkItem { geometry: &poly, tile_x: 0, tile_y: 0, zoom: 0, extent: 4096 };
//! let clipped = Clipper::new().clip_batch(&[item])?;
//! assert_eq!(clipped.len(), 1);
//! # Ok::<(), osmic_accel::AccelError>(())
//! ```

mod accelerator;
mod clip;
mod cpu;
mod error;
#[cfg(osmic_metallib)]
mod metal;
mod prepare;

pub use accelerator::{Backend, Clipper, GpuAccelerator, PendingBatch, is_available};
pub use clip::{ClipOptions, ClippedGeometry, ClippedPolygon, MAX_ZOOM, WorkItem, clip_batch_cpu};
pub use error::{AccelError, AccelResult};
