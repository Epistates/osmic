//! Metal implementation behind [`crate::GpuAccelerator`].

use std::sync::Arc;

use tracing::{debug, info};

use crate::clip::{
    ClipOptions, ClippedGeometry, UnitResults, UnitView, WorkItem, assemble, clip_item_cpu,
    with_item_index,
};
use crate::error::AccelResult;
use crate::prepare::{Plan, Prepared};

use super::batch::{GpuBatch, InFlight};
use super::context::MetalContext;

pub(crate) struct Accelerator {
    ctx: Arc<MetalContext>,
    options: ClipOptions,
}

/// Results for batches with no GPU work (points only).
struct NoUnits;

impl UnitResults for NoUnits {
    fn unit(&self, _index: usize) -> UnitView<'_> {
        UnitView {
            points: &[],
            parts: &[],
        }
    }
}

pub(crate) enum Pending<'a> {
    /// Nothing needed the GPU; results are final.
    Ready(Vec<ClippedGeometry>),
    Running {
        in_flight: InFlight,
        plans: Vec<Plan>,
        items: &'a [WorkItem<'a>],
        options: ClipOptions,
    },
}

impl Accelerator {
    pub(crate) fn is_available() -> bool {
        MetalContext::get().is_ok()
    }

    pub(crate) fn new(options: ClipOptions) -> AccelResult<Self> {
        options.validate()?;
        let ctx = MetalContext::get()?;
        info!("GPU accelerator initialized");
        Ok(Self { ctx, options })
    }

    pub(crate) fn clip_async<'a>(&self, items: &'a [WorkItem<'a>]) -> AccelResult<Pending<'a>> {
        let mut prepared = Prepared::default();
        for (index, item) in items.iter().enumerate() {
            prepared
                .push_item(item, &self.options)
                .map_err(|e| with_item_index(e, index))?;
        }

        match GpuBatch::upload(&self.ctx, &prepared, &self.options)? {
            None => Ok(Pending::Ready(
                prepared
                    .plans
                    .iter()
                    .map(|plan| assemble(plan, &NoUnits))
                    .collect(),
            )),
            Some(batch) => {
                let in_flight = batch.dispatch()?;
                Ok(Pending::Running {
                    in_flight,
                    plans: prepared.plans,
                    items,
                    options: self.options.clone(),
                })
            }
        }
    }
}

impl Pending<'_> {
    pub(crate) fn wait(self) -> AccelResult<Vec<ClippedGeometry>> {
        match self {
            Pending::Ready(results) => Ok(results),
            Pending::Running {
                in_flight,
                plans,
                items,
                options,
            } => {
                let completed = in_flight.wait()?;
                let mut fallbacks = 0usize;
                let mut out = Vec::with_capacity(plans.len());
                for (index, plan) in plans.iter().enumerate() {
                    if plan.unit_range().any(|u| completed.overflowed(u)) {
                        // The GPU ran out of per-ring capacity for this item:
                        // recompute it exactly on the CPU instead of
                        // returning truncated geometry.
                        fallbacks += 1;
                        out.push(clip_item_cpu(&items[index], &options)?);
                    } else {
                        out.push(assemble(plan, &completed));
                    }
                }
                if fallbacks > 0 {
                    debug!(
                        fallbacks,
                        items = plans.len(),
                        "GPU capacity overflow; items recomputed on CPU"
                    );
                }
                Ok(out)
            }
        }
    }
}
