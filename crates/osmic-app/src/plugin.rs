//! The [`Plugin`] trait and plugin groups.

use std::any::TypeId;
use std::sync::Arc;

use crate::app::App;
use crate::error::BoxError;

/// A composable unit of functionality.
///
/// Plugins configure an [`App`]: they insert resources, subscribe to events,
/// add other plugins and may install the runner. The lifecycle is
///
/// 1. [`build`](Self::build), in registration order. Plugins added during
///    this phase are built too, after the ones already registered.
/// 2. [`finish`](Self::finish), in the same order, once every plugin is
///    built: the place to look up resources other plugins provide (see
///    [`App::resource`]).
/// 3. The runner (see [`App::set_runner`]).
/// 4. [`cleanup`](Self::cleanup), in reverse order, exactly once for every
///    plugin whose `build` succeeded — also when a later step fails. A
///    plugin whose `build` fails is not cleaned up, so it should release
///    what it acquired before returning the error.
///
/// Hooks must not call [`App::run`] or [`App::cleanup`]; doing so makes the
/// build fail with [`AppError::Reentrant`](crate::AppError::Reentrant).
///
/// A plugin type is registered at most once per app; later instances of the
/// same type are ignored.
pub trait Plugin: Send + Sync + 'static {
    /// Configure the app.
    ///
    /// # Errors
    ///
    /// A failure aborts [`App::build`] with [`AppError::Plugin`](crate::AppError::Plugin).
    fn build(&self, app: &mut App) -> Result<(), BoxError>;

    /// Finalize once every plugin is built.
    ///
    /// # Errors
    ///
    /// A failure aborts [`App::build`] with [`AppError::Plugin`](crate::AppError::Plugin).
    fn finish(&self, _app: &mut App) -> Result<(), BoxError> {
        Ok(())
    }

    /// Release anything acquired in `build` or `finish`.
    fn cleanup(&self, _app: &mut App) {}

    /// Name used in logs and errors (defaults to the type name).
    fn name(&self) -> &str {
        std::any::type_name::<Self>()
    }
}

/// A registered plugin together with the type it was registered as.
pub(crate) struct Registered {
    pub(crate) type_id: TypeId,
    pub(crate) plugin: Arc<dyn Plugin>,
}

impl Registered {
    pub(crate) fn new<P: Plugin>(plugin: P) -> Self {
        Self {
            type_id: TypeId::of::<P>(),
            plugin: Arc::new(plugin),
        }
    }
}

/// A set of plugins added together with [`App::add_plugins`].
pub trait PluginGroup {
    /// The group's plugins, in the order they should be registered.
    fn build(self) -> PluginGroupBuilder;
}

/// An ordered list of plugins, one per type.
#[derive(Default)]
pub struct PluginGroupBuilder {
    plugins: Vec<Registered>,
}

impl PluginGroupBuilder {
    /// An empty group.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append `plugin`, or replace the plugin of the same type in place.
    #[must_use]
    pub fn add_plugin<P: Plugin>(mut self, plugin: P) -> Self {
        let entry = Registered::new(plugin);
        match self.plugins.iter_mut().find(|r| r.type_id == entry.type_id) {
            Some(existing) => *existing = entry,
            None => self.plugins.push(entry),
        }
        self
    }

    /// Remove the plugin of type `P`, if present.
    #[must_use]
    pub fn disable<P: Plugin>(mut self) -> Self {
        self.plugins.retain(|r| r.type_id != TypeId::of::<P>());
        self
    }

    /// Whether the group contains a plugin of type `P`.
    pub fn contains<P: Plugin>(&self) -> bool {
        self.plugins.iter().any(|r| r.type_id == TypeId::of::<P>())
    }

    pub(crate) fn into_registered(self) -> Vec<Registered> {
        self.plugins
    }
}

impl std::fmt::Debug for PluginGroupBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(self.plugins.iter().map(|r| r.plugin.name()))
            .finish()
    }
}
