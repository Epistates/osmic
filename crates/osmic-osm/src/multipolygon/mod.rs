//! Segment-oriented multipolygon assembly for OSM relation members.
//!
//! This module replaces the earlier `chain_ways()` helper in `pipeline.rs`
//! with a robust algorithm inspired by libosmium's `MultipolygonManager`
//! (Boost Software License 1.0 — pattern-level reference only).
//!
//! The public entry point is [`assemble_multipolygon`]. See the
//! [`assembler`] module docs for the pipeline description.

pub mod assembler;
pub mod ring;
pub mod segment;

pub use assembler::{AssemblyError, AssemblyReport, AssemblyWarning, assemble_multipolygon};
pub use ring::ProtoRing;
pub use segment::{NodeRefSegment, Role, SegmentList};
