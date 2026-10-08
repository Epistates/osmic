//! Removal of temporary files when the process is interrupted.
//!
//! Library writers create temporaries named with
//! [`osmic_core::fs::temp_file_prefix`] next to their outputs and remove
//! them on drop — but destructors do not run when a signal terminates the
//! process. The handler installed here (SIGINT, SIGTERM and SIGHUP on
//! Unix; console events on Windows) deletes this process's temporaries in
//! every registered directory, then exits with the shell's status for the
//! signal: 128 + its number (130 for Ctrl-C, 143 for SIGTERM).
//!
//! `osmic serve` does not install it: the server handles SIGINT/SIGTERM
//! itself and drains before exiting.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::Context;

static DIRS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// Watch `dir` for this process's temporary files.
pub fn register_dir(dir: impl Into<PathBuf>) {
    if let Ok(mut dirs) = DIRS.lock() {
        dirs.push(dir.into());
    }
}

/// Watch the directory where the temporary file for `output` will be
/// created (see [`osmic_core::fs::temp_dir_for`]) and return it.
pub fn register_output(output: &Path) -> anyhow::Result<PathBuf> {
    let dir = osmic_core::fs::temp_dir_for(output)
        .with_context(|| format!("resolving {}", output.display()))?;
    register_dir(dir.clone());
    Ok(dir)
}

/// Delete this process's temporary files and directories in every
/// registered directory.
pub fn remove_temporaries() {
    if let Ok(dirs) = DIRS.lock() {
        remove_temporaries_in(&dirs, std::process::id());
    }
}

/// Delete temporaries of process `pid` in `dirs`.
fn remove_temporaries_in(dirs: &[PathBuf], pid: u32) {
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            if !osmic_core::fs::is_temp_file_of(&name.to_string_lossy(), pid) {
                continue;
            }
            let path = entry.path();
            let _ = if path.is_dir() {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            };
        }
    }
}

/// Remove temporaries and exit with `status`.
fn interrupted(status: i32) -> ! {
    eprintln!("\ninterrupted; removing temporary files");
    remove_temporaries();
    std::process::exit(status);
}

/// Exit status after `signal`, as shells report it.
#[cfg(unix)]
fn exit_status(signal: i32) -> i32 {
    128 + signal
}

/// Install the interrupt handler (once, at startup).
#[cfg(unix)]
pub fn install() {
    use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGTERM};

    let mut signals = match signal_hook::iterator::Signals::new([SIGINT, SIGTERM, SIGHUP]) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "could not install the interrupt handler");
            return;
        }
    };
    let spawned = std::thread::Builder::new()
        .name("osmic-signals".into())
        .spawn(move || {
            if let Some(signal) = signals.forever().next() {
                interrupted(exit_status(signal));
            }
        });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "could not start the interrupt handler");
    }
}

/// Install the interrupt handler (once, at startup).
#[cfg(not(unix))]
pub fn install() {
    // Console events (Ctrl-C, Ctrl-Break, closing the window) are reported
    // as an interrupt.
    if let Err(e) = ctrlc::set_handler(|| interrupted(130)) {
        tracing::warn!(error = %e, "could not install the interrupt handler");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn exit_status_follows_the_shell_convention() {
        use signal_hook::consts::signal::{SIGINT, SIGTERM};
        assert_eq!(exit_status(SIGINT), 130);
        assert_eq!(exit_status(SIGTERM), 143);
    }

    #[test]
    fn bare_output_names_register_the_current_directory() {
        assert_eq!(
            register_output(Path::new("planet.osm.pbf")).expect("register"),
            Path::new(".")
        );
        assert!(
            DIRS.lock()
                .expect("dirs")
                .iter()
                .any(|d| d == Path::new("."))
        );
    }

    #[test]
    fn removes_only_this_process_temporaries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mine = dir.path().join(format!(
            "{}x.pmtiles.tmp",
            osmic_core::fs::temp_file_prefix()
        ));
        let mine_dir = dir
            .path()
            .join(format!("{}sort", osmic_core::fs::temp_file_prefix()));
        let other = dir.path().join(".osmic-1-x.tmp");
        let output = dir.path().join("out.pmtiles");
        std::fs::write(&mine, b"x").expect("write");
        std::fs::create_dir(&mine_dir).expect("mkdir");
        std::fs::write(&other, b"x").expect("write");
        std::fs::write(&output, b"x").expect("write");
        remove_temporaries_in(&[dir.path().to_path_buf()], std::process::id());
        assert!(!mine.exists() && !mine_dir.exists());
        assert!(other.exists() && output.exists());
    }
}
