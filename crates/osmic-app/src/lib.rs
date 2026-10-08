//! Application framework: plugins, typed resources, events and a runner.
//!
//! An [`App`] is assembled from [`Plugin`]s. Each plugin inserts resources,
//! subscribes to events, adds other plugins or installs the runner; the app
//! then builds and finishes every plugin in a fixed order, runs, and cleans
//! up — also when a step fails. See [`Plugin`] for the lifecycle and
//! [`App`] for an example.

#![warn(missing_docs)]

pub mod app;
mod error;
pub mod event;
pub mod plugin;
pub mod resource;

pub use app::App;
pub use error::{AppError, BoxError, Phase};
pub use event::{Event, EventBus};
pub use plugin::{Plugin, PluginGroup, PluginGroupBuilder};
pub use resource::Resources;
