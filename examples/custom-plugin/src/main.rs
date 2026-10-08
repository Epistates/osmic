//! Compose an app from plugins: one provides a resource, another depends
//! on it in `finish`, events connect them, and a runner does the work.
//!
//! ```sh
//! cargo run -p custom-plugin
//! ```

use std::sync::{Arc, Mutex};

use osmic_app::{App, AppError, BoxError, Event, Plugin};

/// Shared configuration provided by [`ConfigPlugin`].
struct Config {
    greeting: String,
}

struct ConfigPlugin;

impl Plugin for ConfigPlugin {
    fn build(&self, app: &mut App) -> Result<(), BoxError> {
        app.insert_resource(Config {
            greeting: "hello from osmic".into(),
        });
        Ok(())
    }
}

/// Emitted by the runner for every greeting it prints.
struct Greeted(String);
impl Event for Greeted {}

/// Records greetings and prints a summary on cleanup.
struct GreeterPlugin {
    seen: Arc<Mutex<Vec<String>>>,
}

impl Plugin for GreeterPlugin {
    fn build(&self, app: &mut App) -> Result<(), BoxError> {
        // Plugins can depend on other plugins by adding them.
        app.add_plugin(ConfigPlugin);
        let seen = Arc::clone(&self.seen);
        app.subscribe(move |g: &Greeted| seen.lock().expect("log").push(g.0.clone()));
        app.set_runner(|app| {
            let greeting = app.resource::<Config>()?.greeting.clone();
            println!("{greeting}");
            app.emit(&Greeted(greeting));
            Ok(())
        });
        Ok(())
    }

    fn finish(&self, app: &mut App) -> Result<(), BoxError> {
        // Every plugin is built by now, so dependencies can be checked.
        app.resource::<Config>()?;
        Ok(())
    }

    fn cleanup(&self, _app: &mut App) {
        println!("greetings sent: {}", self.seen.lock().expect("log").len());
    }
}

fn main() -> Result<(), AppError> {
    App::new()
        .add_plugin(GreeterPlugin {
            seen: Arc::default(),
        })
        .run()
}
