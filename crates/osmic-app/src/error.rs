use std::fmt;

/// Boxed error returned by plugin hooks and runners.
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// Lifecycle phase in which a plugin failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Phase {
    /// [`Plugin::build`](crate::Plugin::build).
    Build,
    /// [`Plugin::finish`](crate::Plugin::finish).
    Finish,
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Build => "build",
            Self::Finish => "finish",
        })
    }
}

/// Errors from building or running an [`App`](crate::App).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AppError {
    /// A plugin's `build` or `finish` hook failed.
    #[error("plugin {plugin} failed during {phase}: {source}")]
    Plugin {
        plugin: String,
        phase: Phase,
        #[source]
        source: BoxError,
    },
    /// A resource that a plugin depends on was never inserted.
    #[error("missing resource {resource}; add the plugin that provides it")]
    MissingResource { resource: &'static str },
    /// A plugin was added after the build phase, so it would never run.
    #[error("plugin {plugin} was added after the app was built")]
    AddedAfterBuild { plugin: String },
    /// The app was used after [`App::cleanup`](crate::App::cleanup).
    #[error("the app has already been cleaned up")]
    CleanedUp,
    /// [`App::run`](crate::App::run) or [`App::cleanup`](crate::App::cleanup)
    /// was called from a plugin's `build` or `finish` hook, while the app
    /// was still being built.
    #[error("App::{method} was called from a plugin's build or finish hook")]
    Reentrant { method: &'static str },
    /// The runner failed.
    #[error("runner failed: {0}")]
    Runner(#[source] BoxError),
}
