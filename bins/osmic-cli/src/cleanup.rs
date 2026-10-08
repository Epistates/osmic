//! Removal of temporary files when the process is interrupted.
//!
//! Library writers create temporaries named with
//! [`osmic_core::fs::temp_file_prefix`] next to their outputs and remove
//! them on drop — but destructors do not run when a signal terminates the
//! process. The Ctrl-C/SIGTERM handler installed here deletes this
//! process's temporaries in every registered directory, then exits with
//! status 130.

use std::path::PathBuf;
use std::sync::Mutex;

static DIRS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// Watch `dir` for this process's temporary files.
pub fn register_dir(dir: impl Into<PathBuf>) {
    if let Ok(mut dirs) = DIRS.lock() {
        dirs.push(dir.into());
    }
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

/// Install the interrupt handler (once, at startup).
pub fn install() {
    let result = ctrlc::set_handler(|| {
        eprintln!("\ninterrupted; removing temporary files");
        remove_temporaries();
        std::process::exit(130);
    });
    if let Err(e) = result {
        tracing::warn!(error = %e, "could not install the interrupt handler");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
