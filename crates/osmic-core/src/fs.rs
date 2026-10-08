//! Filesystem conventions shared by osmic writers.
//!
//! Outputs are written to a temporary file next to the destination
//! ([`temp_file_for`]) and moved into place when complete ([`persist`]), so
//! readers never see a partial file and an interrupted run leaves the old
//! file untouched.

use std::io;
use std::path::Path;

use tempfile::NamedTempFile;
use tracing::warn;

/// Prefix for temporary files that osmic creates next to an output while
/// writing it (they are renamed over the destination when complete).
///
/// The process id is included so a signal handler can remove exactly the
/// files of the process being interrupted.
pub fn temp_file_prefix() -> String {
    format!(".osmic-{}-", std::process::id())
}

/// Whether `file_name` is a temporary file created by process `pid`.
pub fn is_temp_file_of(file_name: &str, pid: u32) -> bool {
    file_name.starts_with(&format!(".osmic-{pid}-"))
}

/// Create a temporary file in the directory of `path`, so it can later be
/// renamed over `path` without crossing filesystems.
pub fn temp_file_for(path: &Path) -> io::Result<NamedTempFile> {
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    tempfile::Builder::new()
        .prefix(&temp_file_prefix())
        .suffix(".tmp")
        .tempfile_in(dir)
}

/// Flush `temp` to stable storage and move it to `path`.
///
/// Without `overwrite` an existing `path` is never replaced (the call fails
/// with [`io::ErrorKind::AlreadyExists`]). On filesystems that cannot
/// rename without replacing (exFAT, FAT, some network mounts) that check
/// and the rename are separate steps, and on filesystems that cannot flush
/// to stable storage the flush is skipped with a warning; the move itself
/// is atomic either way.
pub fn persist(temp: NamedTempFile, path: &Path, overwrite: bool) -> io::Result<()> {
    if let Err(e) = temp.as_file().sync_all() {
        if !is_unsupported(&e) {
            return Err(e);
        }
        warn!(path = %path.display(), "Filesystem cannot flush to stable storage; continuing without fsync");
    }
    if overwrite {
        return temp.persist(path).map(drop).map_err(|e| e.error);
    }
    match temp.persist_noclobber(path) {
        Ok(_) => Ok(()),
        Err(e) if is_unsupported(&e.error) => {
            if std::fs::symlink_metadata(path).is_ok() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{} already exists", path.display()),
                ));
            }
            e.file.persist(path).map(drop).map_err(|e| e.error)
        }
        Err(e) => Err(e.error),
    }
}

/// The operation is not implemented by the filesystem.
fn is_unsupported(e: &io::Error) -> bool {
    if e.kind() == io::ErrorKind::Unsupported {
        return true;
    }
    #[cfg(unix)]
    if let Some(code) = e.raw_os_error() {
        return code == libc::ENOTSUP || code == libc::EOPNOTSUPP;
    }
    false
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    #[test]
    fn prefix_identifies_this_process() {
        let name = format!("{}abc.pmtiles.tmp", temp_file_prefix());
        assert!(is_temp_file_of(&name, std::process::id()));
        assert!(!is_temp_file_of(&name, std::process::id().wrapping_add(1)));
    }

    #[test]
    fn persist_respects_overwrite() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("out.bin");
        let write = |bytes: &[u8], overwrite| {
            let mut temp = temp_file_for(&path).expect("temp");
            temp.write_all(bytes).expect("write");
            persist(temp, &path, overwrite)
        };
        write(b"one", false).expect("new file");
        let err = write(b"two", false).expect_err("exists");
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&path).expect("read"), b"one");
        write(b"three", true).expect("overwrite");
        assert_eq!(std::fs::read(&path).expect("read"), b"three");
        // Only the destination is left.
        assert_eq!(std::fs::read_dir(dir.path()).expect("dir").count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn enotsup_is_unsupported() {
        assert!(is_unsupported(&io::Error::from_raw_os_error(libc::ENOTSUP)));
        assert!(!is_unsupported(&io::Error::from_raw_os_error(libc::EACCES)));
    }
}
