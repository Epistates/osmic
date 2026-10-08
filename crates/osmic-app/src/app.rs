use std::any::TypeId;
use std::collections::HashSet;

use tracing::info;

use crate::event::EventBus;
use crate::plugin::{Plugin, PluginGroup};
use crate::resource::Resources;

/// The central application container.
///
/// Holds plugins, resources, and the event bus. Follows a build lifecycle:
/// 1. Create `App::new()`
/// 2. Add plugins via `add_plugin()` / `add_plugins()`
/// 3. Insert resources via `insert_resource()`
/// 4. Call `build()` to initialize all plugins
/// 5. Let the owning binary or plugin drive any long-running loop
pub struct App {
    pub resources: Resources,
    pub event_bus: EventBus,
    plugins: Vec<Box<dyn Plugin>>,
    plugin_names: HashSet<TypeId>,
    built: bool,
}

impl App {
    pub fn new() -> Self {
        Self {
            resources: Resources::new(),
            event_bus: EventBus::new(),
            plugins: Vec::new(),
            plugin_names: HashSet::new(),
            built: false,
        }
    }

    /// Add a single plugin. Deduplicates by type.
    pub fn add_plugin<P: Plugin>(&mut self, plugin: P) -> &mut Self {
        let type_id = TypeId::of::<P>();
        if self.plugin_names.contains(&type_id) {
            info!(
                plugin = plugin.name(),
                "Plugin already registered, skipping"
            );
            return self;
        }
        self.plugin_names.insert(type_id);
        self.plugins.push(Box::new(plugin));
        self
    }

    /// Add a group of plugins. Deduplicates by plugin name.
    pub fn add_plugins<G: PluginGroup>(&mut self, group: G) -> &mut Self {
        let builder = group.build();
        for plugin in builder.into_plugins() {
            let name = plugin.name().to_string();
            // Deduplicate: skip if a plugin with the same name is already registered
            if self.plugins.iter().any(|p| p.name() == name) {
                info!(
                    plugin = name,
                    "Plugin already registered via group, skipping"
                );
                continue;
            }
            self.plugins.push(plugin);
            info!(plugin = name, "Added plugin from group");
        }
        self
    }

    /// Insert a typed resource into the app.
    pub fn insert_resource<R: Send + Sync + 'static>(&mut self, resource: R) -> &mut Self {
        self.resources.insert(resource);
        self
    }

    /// Get an immutable reference to a resource.
    pub fn get_resource<R: Send + Sync + 'static>(&self) -> Option<&R> {
        self.resources.get::<R>()
    }

    /// Get a mutable reference to a resource.
    pub fn get_resource_mut<R: Send + Sync + 'static>(&mut self) -> Option<&mut R> {
        self.resources.get_mut::<R>()
    }

    /// Build all plugins: calls `build()`, waits for `ready()`, then `finish()`.
    pub fn build(&mut self) {
        if self.built {
            return;
        }

        // Take plugins out to avoid borrow issues
        let plugins = std::mem::take(&mut self.plugins);

        // Build phase
        for plugin in &plugins {
            info!(plugin = plugin.name(), "Building plugin");
            plugin.build(self);
        }

        // Ready check
        for plugin in &plugins {
            if !plugin.ready(self) {
                info!(plugin = plugin.name(), "Plugin not ready");
            }
        }

        // Finish phase
        for plugin in &plugins {
            plugin.finish(self);
        }

        self.plugins = plugins;
        self.built = true;
    }

    /// Cleanup all plugins (call before dropping).
    pub fn cleanup(&mut self) {
        let plugins = std::mem::take(&mut self.plugins);
        for plugin in &plugins {
            plugin.cleanup(self);
        }
        self.plugins = plugins;
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use crate::plugin::Plugin;

    // ---- helper types --------------------------------------------------

    /// Counts how many times `build()` is called on the plugin.
    struct CountingPlugin {
        build_calls: Arc<AtomicU32>,
    }

    impl CountingPlugin {
        fn new(counter: Arc<AtomicU32>) -> Self {
            Self {
                build_calls: counter,
            }
        }
    }

    impl Plugin for CountingPlugin {
        fn build(&self, _app: &mut App) {
            self.build_calls.fetch_add(1, Ordering::SeqCst);
        }
        fn name(&self) -> &str {
            "CountingPlugin"
        }
    }

    /// A resource-inserting plugin used to verify that `build()` truly runs.
    struct MarkerResource;

    struct InsertingPlugin;

    impl Plugin for InsertingPlugin {
        fn build(&self, app: &mut App) {
            app.insert_resource(MarkerResource);
        }
        fn name(&self) -> &str {
            "InsertingPlugin"
        }
    }

    // ---- tests ---------------------------------------------------------

    #[test]
    fn new_creates_empty_app() {
        let app = App::new();
        // No resources, no plugins — just verify construction succeeds and
        // an arbitrary resource lookup returns None.
        assert!(app.get_resource::<MarkerResource>().is_none());
    }

    #[test]
    fn add_plugin_deduplicates_by_typeid() {
        let counter = Arc::new(AtomicU32::new(0));
        let mut app = App::new();
        app.add_plugin(CountingPlugin::new(Arc::clone(&counter)));
        // Adding a second instance of the same type must be a no-op.
        app.add_plugin(CountingPlugin::new(Arc::clone(&counter)));
        app.build();
        // Only one plugin was registered, so build() fires exactly once.
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn insert_and_get_resource_round_trip() {
        let mut app = App::new();
        app.insert_resource(42u32);
        assert_eq!(app.get_resource::<u32>(), Some(&42u32));
    }

    #[test]
    fn build_calls_plugin_build() {
        let mut app = App::new();
        app.add_plugin(InsertingPlugin);
        // Before build, the resource must not exist.
        assert!(app.get_resource::<MarkerResource>().is_none());
        app.build();
        // After build, the resource inserted by the plugin must be present.
        assert!(app.get_resource::<MarkerResource>().is_some());
    }

    #[test]
    fn build_is_idempotent() {
        let counter = Arc::new(AtomicU32::new(0));
        let mut app = App::new();
        app.add_plugin(CountingPlugin::new(Arc::clone(&counter)));

        app.build();
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "first build should fire once"
        );

        app.build(); // second call must be a no-op
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "second build must not re-fire plugins"
        );
    }
}
