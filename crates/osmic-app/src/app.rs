use std::any::TypeId;
use std::sync::Arc;

use tracing::{debug, warn};

use crate::error::{AppError, BoxError, Phase};
use crate::event::{Event, EventBus};
use crate::plugin::{Plugin, PluginGroup, Registered};
use crate::resource::Resources;

type Runner = Box<dyn FnOnce(&mut App) -> Result<(), BoxError>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Configuring,
    Building,
    Finishing,
    Built,
    CleanedUp,
}

/// The application container: plugins, resources, events and a runner.
///
/// ```
/// use osmic_app::{App, BoxError, Plugin};
///
/// struct Greeting(&'static str);
///
/// struct GreetingPlugin;
/// impl Plugin for GreetingPlugin {
///     fn build(&self, app: &mut App) -> Result<(), BoxError> {
///         app.insert_resource(Greeting("hello"));
///         Ok(())
///     }
/// }
///
/// let mut app = App::new();
/// app.add_plugin(GreetingPlugin).set_runner(|app| {
///     assert_eq!(app.resource::<Greeting>()?.0, "hello");
///     Ok(())
/// });
/// app.run()?;
/// # Ok::<(), osmic_app::AppError>(())
/// ```
///
/// See [`Plugin`] for the lifecycle.
pub struct App {
    resources: Resources,
    events: EventBus,
    plugins: Vec<Registered>,
    /// Plugins `plugins[..built]` have been built successfully.
    built: usize,
    state: State,
    runner: Option<Runner>,
    /// First misuse detected by a chainable method, reported by `build`.
    deferred: Option<AppError>,
}

impl App {
    pub fn new() -> Self {
        Self {
            resources: Resources::new(),
            events: EventBus::new(),
            plugins: Vec::new(),
            built: 0,
            state: State::Configuring,
            runner: None,
            deferred: None,
        }
    }

    /// Register a plugin. A second plugin of the same type is ignored.
    ///
    /// Plugins may be added before [`build`](Self::build) or from another
    /// plugin's [`Plugin::build`]; adding one later makes `build`/`run` fail
    /// with [`AppError::AddedAfterBuild`].
    pub fn add_plugin<P: Plugin>(&mut self, plugin: P) -> &mut Self {
        self.register(Registered::new(plugin));
        self
    }

    /// Register every plugin of a group, in order, with the same rules as
    /// [`add_plugin`](Self::add_plugin).
    pub fn add_plugins<G: PluginGroup>(&mut self, group: G) -> &mut Self {
        for entry in group.build().into_registered() {
            self.register(entry);
        }
        self
    }

    fn register(&mut self, entry: Registered) {
        if !matches!(self.state, State::Configuring | State::Building) {
            self.deferred.get_or_insert(AppError::AddedAfterBuild {
                plugin: entry.plugin.name().to_string(),
            });
            return;
        }
        if self.plugins.iter().any(|r| r.type_id == entry.type_id) {
            debug!(
                plugin = entry.plugin.name(),
                "Plugin already registered; ignoring"
            );
            return;
        }
        self.plugins.push(entry);
    }

    /// Whether a plugin of type `P` is registered.
    pub fn has_plugin<P: Plugin>(&self) -> bool {
        self.plugins.iter().any(|r| r.type_id == TypeId::of::<P>())
    }

    /// Names of the registered plugins, in registration order.
    pub fn plugin_names(&self) -> impl Iterator<Item = &str> {
        self.plugins.iter().map(|r| r.plugin.name())
    }

    /// Insert a resource, replacing any existing value of the same type.
    pub fn insert_resource<R: Send + Sync + 'static>(&mut self, resource: R) -> &mut Self {
        self.resources.insert(resource);
        self
    }

    pub fn get_resource<R: Send + Sync + 'static>(&self) -> Option<&R> {
        self.resources.get::<R>()
    }

    pub fn get_resource_mut<R: Send + Sync + 'static>(&mut self) -> Option<&mut R> {
        self.resources.get_mut::<R>()
    }

    /// A resource another plugin must have provided.
    ///
    /// # Errors
    ///
    /// [`AppError::MissingResource`] naming the type if it is absent.
    pub fn resource<R: Send + Sync + 'static>(&self) -> Result<&R, AppError> {
        self.resources.get::<R>().ok_or_else(missing::<R>)
    }

    /// Mutable form of [`resource`](Self::resource).
    ///
    /// # Errors
    ///
    /// [`AppError::MissingResource`] naming the type if it is absent.
    pub fn resource_mut<R: Send + Sync + 'static>(&mut self) -> Result<&mut R, AppError> {
        self.resources.get_mut::<R>().ok_or_else(missing::<R>)
    }

    pub fn remove_resource<R: Send + Sync + 'static>(&mut self) -> Option<R> {
        self.resources.remove::<R>()
    }

    pub fn contains_resource<R: Send + Sync + 'static>(&self) -> bool {
        self.resources.contains::<R>()
    }

    /// Call `handler` for every event of type `E` emitted on this app.
    pub fn subscribe<E: Event, F>(&mut self, handler: F) -> &mut Self
    where
        F: Fn(&E) + Send + Sync + 'static,
    {
        self.events.subscribe(handler);
        self
    }

    /// Deliver `event` to its subscribers, in subscription order.
    pub fn emit<E: Event>(&self, event: &E) {
        self.events.emit(event);
    }

    /// Set the function [`run`](Self::run) calls after building, typically a
    /// server or event loop. Without one, `run` builds and cleans up.
    pub fn set_runner<F>(&mut self, runner: F) -> &mut Self
    where
        F: FnOnce(&mut App) -> Result<(), BoxError> + 'static,
    {
        if self.runner.is_some() {
            warn!("Replacing the app runner set by an earlier plugin");
        }
        self.runner = Some(Box::new(runner));
        self
    }

    /// Build and finish every plugin (see [`Plugin`]). Idempotent.
    ///
    /// On failure the plugins built so far are cleaned up and the app can no
    /// longer be used.
    ///
    /// # Errors
    ///
    /// The first [`AppError`] from a plugin hook or from misuse of the app.
    pub fn build(&mut self) -> Result<(), AppError> {
        match self.state {
            State::Configuring => {}
            State::Built => return self.deferred.take().map_or(Ok(()), Err),
            // Called from inside a plugin hook: the outer call finishes the job.
            State::Building | State::Finishing => return Ok(()),
            State::CleanedUp => return Err(AppError::CleanedUp),
        }
        if let Some(err) = self.deferred.take() {
            self.cleanup();
            return Err(err);
        }
        self.state = State::Building;
        // Indexing (not iterating) picks up plugins added during the loop.
        while self.built < self.plugins.len() {
            let plugin = Arc::clone(&self.plugins[self.built].plugin);
            debug!(plugin = plugin.name(), "Building plugin");
            plugin
                .build(self)
                .map_err(|source| self.abort(Phase::Build, &*plugin, source))?;
            self.built += 1;
        }
        self.state = State::Finishing;
        for i in 0..self.plugins.len() {
            let plugin = Arc::clone(&self.plugins[i].plugin);
            plugin
                .finish(self)
                .map_err(|source| self.abort(Phase::Finish, &*plugin, source))?;
        }
        if let Some(err) = self.deferred.take() {
            self.cleanup();
            return Err(err);
        }
        self.state = State::Built;
        Ok(())
    }

    fn abort(&mut self, phase: Phase, plugin: &dyn Plugin, source: BoxError) -> AppError {
        self.cleanup();
        AppError::Plugin {
            plugin: plugin.name().to_string(),
            phase,
            source,
        }
    }

    /// Build if needed, call the runner, then clean up — also on failure.
    ///
    /// # Errors
    ///
    /// The build error, or [`AppError::Runner`] wrapping the runner's error.
    pub fn run(&mut self) -> Result<(), AppError> {
        let result = self.build().and_then(|()| match self.runner.take() {
            Some(runner) => runner(self).map_err(AppError::Runner),
            None => Ok(()),
        });
        self.cleanup();
        result
    }

    /// Clean up every built plugin in reverse order. Runs at most once; also
    /// called by [`run`](Self::run) and on drop.
    pub fn cleanup(&mut self) {
        if self.state == State::CleanedUp {
            return;
        }
        self.state = State::CleanedUp;
        for i in (0..self.built).rev() {
            let plugin = Arc::clone(&self.plugins[i].plugin);
            debug!(plugin = plugin.name(), "Cleaning up plugin");
            plugin.cleanup(self);
        }
    }
}

fn missing<R>() -> AppError {
    AppError::MissingResource {
        resource: std::any::type_name::<R>(),
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.cleanup();
    }
}

impl std::fmt::Debug for App {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("App")
            .field("plugins", &self.plugin_names().collect::<Vec<_>>())
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::plugin::PluginGroupBuilder;

    /// Shared record of lifecycle calls, in order.
    #[derive(Clone, Default)]
    struct Log(Arc<Mutex<Vec<String>>>);

    impl Log {
        fn push(&self, entry: impl Into<String>) {
            self.0.lock().expect("log").push(entry.into());
        }
        fn take(&self) -> Vec<String> {
            std::mem::take(&mut *self.0.lock().expect("log"))
        }
    }

    /// Records every hook; optionally fails one phase.
    struct Probe<const N: u8> {
        log: Log,
        fail: Option<Phase>,
    }

    impl<const N: u8> Probe<N> {
        fn new(log: &Log) -> Self {
            Self {
                log: log.clone(),
                fail: None,
            }
        }
        fn failing(log: &Log, phase: Phase) -> Self {
            Self {
                log: log.clone(),
                fail: Some(phase),
            }
        }
    }

    impl<const N: u8> Plugin for Probe<N> {
        fn build(&self, _app: &mut App) -> Result<(), BoxError> {
            self.log.push(format!("build {N}"));
            match self.fail {
                Some(Phase::Build) => Err("boom".into()),
                _ => Ok(()),
            }
        }
        fn finish(&self, _app: &mut App) -> Result<(), BoxError> {
            self.log.push(format!("finish {N}"));
            match self.fail {
                Some(Phase::Finish) => Err("boom".into()),
                _ => Ok(()),
            }
        }
        fn cleanup(&self, _app: &mut App) {
            self.log.push(format!("cleanup {N}"));
        }
        fn name(&self) -> &str {
            "Probe"
        }
    }

    #[test]
    fn lifecycle_order_and_single_cleanup() {
        let log = Log::default();
        let mut app = App::new();
        app.add_plugin(Probe::<1>::new(&log))
            .add_plugin(Probe::<2>::new(&log));
        let runner_log = log.clone();
        app.set_runner(move |_| {
            runner_log.push("run");
            Ok(())
        });
        app.run().expect("runs");
        drop(app);
        assert_eq!(
            log.take(),
            [
                "build 1",
                "build 2",
                "finish 1",
                "finish 2",
                "run",
                "cleanup 2",
                "cleanup 1"
            ]
        );
    }

    #[test]
    fn duplicates_are_ignored_across_add_plugin_and_groups() {
        struct Group(Log);
        impl PluginGroup for Group {
            fn build(self) -> PluginGroupBuilder {
                PluginGroupBuilder::new()
                    .add_plugin(Probe::<1>::new(&self.0))
                    .add_plugin(Probe::<2>::new(&self.0))
            }
        }
        let log = Log::default();
        let mut app = App::new();
        app.add_plugin(Probe::<1>::new(&log))
            .add_plugins(Group(log.clone()))
            .add_plugin(Probe::<2>::new(&log));
        assert!(app.has_plugin::<Probe<2>>());
        app.build().expect("builds");
        assert_eq!(log.take(), ["build 1", "build 2", "finish 1", "finish 2"]);
    }

    #[test]
    fn plugins_added_during_build_are_built() {
        struct Parent(Log);
        impl Plugin for Parent {
            fn build(&self, app: &mut App) -> Result<(), BoxError> {
                app.add_plugin(Probe::<7>::new(&self.0));
                Ok(())
            }
        }
        let log = Log::default();
        let mut app = App::new();
        app.add_plugin(Parent(log.clone()));
        app.build().expect("builds");
        assert_eq!(log.take(), ["build 7", "finish 7"]);
        drop(app);
        assert_eq!(log.take(), ["cleanup 7"]);
    }

    #[test]
    fn build_failure_cleans_up_what_was_built() {
        let log = Log::default();
        let mut app = App::new();
        app.add_plugin(Probe::<1>::new(&log))
            .add_plugin(Probe::<2>::failing(&log, Phase::Build))
            .add_plugin(Probe::<3>::new(&log));
        let err = app.build().expect_err("fails");
        assert!(matches!(
            err,
            AppError::Plugin {
                phase: Phase::Build,
                ..
            }
        ));
        assert_eq!(err.to_string(), "plugin Probe failed during build: boom");
        assert_eq!(log.take(), ["build 1", "build 2", "cleanup 1"]);
        assert!(matches!(app.build(), Err(AppError::CleanedUp)));
        drop(app);
        assert!(log.take().is_empty(), "cleanup runs once");
    }

    #[test]
    fn finish_failure_cleans_up_everything() {
        let log = Log::default();
        let mut app = App::new();
        app.add_plugin(Probe::<1>::failing(&log, Phase::Finish))
            .add_plugin(Probe::<2>::new(&log));
        assert!(matches!(
            app.run(),
            Err(AppError::Plugin {
                phase: Phase::Finish,
                ..
            })
        ));
        assert_eq!(
            log.take(),
            ["build 1", "build 2", "finish 1", "cleanup 2", "cleanup 1"]
        );
    }

    #[test]
    fn finish_can_require_resources_from_other_plugins() {
        struct Provider;
        impl Plugin for Provider {
            fn build(&self, app: &mut App) -> Result<(), BoxError> {
                app.insert_resource(41u32);
                Ok(())
            }
        }
        struct Consumer;
        impl Plugin for Consumer {
            fn build(&self, _: &mut App) -> Result<(), BoxError> {
                Ok(())
            }
            fn finish(&self, app: &mut App) -> Result<(), BoxError> {
                *app.resource_mut::<u32>()? += 1;
                Ok(())
            }
        }
        // Consumer is registered first; finish still sees Provider's resource.
        let mut app = App::new();
        app.add_plugin(Consumer).add_plugin(Provider);
        app.build().expect("builds");
        assert_eq!(app.get_resource::<u32>(), Some(&42));

        let mut lonely = App::new();
        lonely.add_plugin(Consumer);
        let err = lonely.build().expect_err("missing provider");
        let AppError::Plugin { source, .. } = err else {
            panic!("unexpected {err:?}");
        };
        assert_eq!(
            source.to_string(),
            "missing resource u32; add the plugin that provides it"
        );
    }

    #[test]
    fn adding_after_build_is_reported() {
        let log = Log::default();
        let mut app = App::new();
        app.build().expect("empty app builds");
        app.add_plugin(Probe::<1>::new(&log));
        assert!(matches!(app.run(), Err(AppError::AddedAfterBuild { .. })));
        assert!(log.take().is_empty());
    }

    #[test]
    fn runner_errors_still_clean_up() {
        let log = Log::default();
        let mut app = App::new();
        app.add_plugin(Probe::<1>::new(&log))
            .set_runner(|_| Err("runner broke".into()));
        let err = app.run().expect_err("runner fails");
        assert_eq!(err.to_string(), "runner failed: runner broke");
        assert_eq!(log.take(), ["build 1", "finish 1", "cleanup 1"]);
    }

    #[test]
    fn group_builder_replaces_and_disables() {
        let log = Log::default();
        let group = PluginGroupBuilder::new()
            .add_plugin(Probe::<1>::new(&log))
            .add_plugin(Probe::<2>::new(&log))
            .add_plugin(Probe::<1>::failing(&log, Phase::Build))
            .disable::<Probe<2>>();
        assert!(group.contains::<Probe<1>>());
        assert!(!group.contains::<Probe<2>>());
        struct Wrap(PluginGroupBuilder);
        impl PluginGroup for Wrap {
            fn build(self) -> PluginGroupBuilder {
                self.0
            }
        }
        let mut app = App::new();
        app.add_plugins(Wrap(group));
        assert!(app.build().is_err(), "replacement instance is the one used");
    }

    #[test]
    fn events_reach_subscribers() {
        struct Ping(u32);
        impl Event for Ping {}
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let mut app = App::new();
        app.subscribe(move |p: &Ping| sink.lock().expect("lock").push(p.0));
        app.emit(&Ping(3));
        app.emit(&Ping(4));
        assert_eq!(*seen.lock().expect("lock"), [3, 4]);
    }
}
