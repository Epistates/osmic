//! Filesystem conventions shared by osmic writers.
//!
//! Outputs are written to a temporary file next to the destination
//! ([`temp_file_for`]) and moved into place when complete ([`persist`]), so
//! readers never see a partial file and an interrupted run leaves the old
//! file untouched.
//!
//! The result looks like a file written in place: a destination that is a
//! symbolic link is followed (the link's target is replaced, the link
//! stays), a replaced file keeps its permissions, and a new file gets the
//! usual `0666 & !umask` mode on Unix rather than the owner-only mode of a
//! temporary file.

use std::io;
use std::path::{Path, PathBuf};

use tempfile::NamedTempFile;
use tracing::warn;

/// Symbolic links followed before giving up (Linux's `MAXSYMLINKS`).
const MAX_SYMLINK_HOPS: usize = 40;

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

/// The file a write to `path` replaces: `path` itself or, when `path` is a
/// symbolic link, the end of the link chain (which need not exist yet).
///
/// # Errors
///
/// Failure to inspect or read a link, or more than 40 links in a chain.
pub fn resolve_destination(path: &Path) -> io::Result<PathBuf> {
    let mut current = path.to_path_buf();
    for _ in 0..MAX_SYMLINK_HOPS {
        match std::fs::symlink_metadata(&current) {
            Ok(m) if m.file_type().is_symlink() => {
                let target = std::fs::read_link(&current)?;
                current = match current.parent() {
                    Some(dir) if target.is_relative() => dir.join(target),
                    _ => target,
                };
            }
            Ok(_) => return Ok(current),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(current),
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("{}: too many levels of symbolic links", path.display()),
    ))
}

/// The directory in which [`temp_file_for`] creates the temporary file for
/// `path` (the directory of the resolved destination).
///
/// # Errors
///
/// As for [`resolve_destination`].
pub fn temp_dir_for(path: &Path) -> io::Result<PathBuf> {
    Ok(parent_dir(&resolve_destination(path)?).to_path_buf())
}

fn parent_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    }
}

/// Create a temporary file in the directory of `path` (after following
/// symbolic links), so it can later be renamed over `path` without crossing
/// filesystems.
pub fn temp_file_for(path: &Path) -> io::Result<NamedTempFile> {
    let prefix = temp_file_prefix();
    let mut builder = tempfile::Builder::new();
    builder.prefix(&prefix).suffix(".tmp");
    // The mode a plain `File::create` would use (the kernel applies the
    // umask), instead of tempfile's owner-only 0600.
    #[cfg(unix)]
    builder.permissions(std::os::unix::fs::PermissionsExt::from_mode(0o666));
    builder.tempfile_in(temp_dir_for(path)?)
}

/// Flush `temp` to stable storage and move it to `path`.
///
/// Without `overwrite` an existing `path` is never replaced (the call fails
/// with [`io::ErrorKind::AlreadyExists`]). A symbolic link at `path` is
/// followed, and a replaced file's permissions carry over. On filesystems
/// that cannot rename without replacing (exFAT, FAT, some network mounts)
/// the existence check and the rename are separate steps, and on
/// filesystems that cannot flush to stable storage the flush is skipped
/// with a warning; the move itself is atomic either way. On Unix the
/// directory is flushed after the rename so the new name is durable too.
pub fn persist(temp: NamedTempFile, path: &Path, overwrite: bool) -> io::Result<()> {
    let path = resolve_destination(path)?;
    if overwrite {
        keep_permissions(&temp, &path)?;
    }
    if let Err(e) = temp.as_file().sync_all() {
        if !is_unsupported(&e) {
            return Err(e);
        }
        warn!(path = %path.display(), "Filesystem cannot flush to stable storage; continuing without fsync");
    }
    if overwrite {
        temp.persist(&path).map_err(|e| e.error)?;
    } else {
        match temp.persist_noclobber(&path) {
            Ok(_) => {}
            Err(e) if is_unsupported(&e.error) => {
                if std::fs::symlink_metadata(&path).is_ok() {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!("{} already exists", path.display()),
                    ));
                }
                e.file.persist(&path).map_err(|e| e.error)?;
            }
            Err(e) => return Err(e.error),
        }
    }
    sync_dir(parent_dir(&path))
}

/// Give `temp` the permissions of the file it is about to replace, if any.
#[cfg(unix)]
fn keep_permissions(temp: &NamedTempFile, path: &Path) -> io::Result<()> {
    let permissions = match std::fs::metadata(path) {
        Ok(m) => m.permissions(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    match temp.as_file().set_permissions(permissions) {
        // FAT-like filesystems have no real modes to keep.
        Err(e) if is_unsupported(&e) || e.kind() == io::ErrorKind::PermissionDenied => {
            warn!(path = %path.display(), error = %e, "Cannot keep the file's permissions");
            Ok(())
        }
        other => other,
    }
}

#[cfg(not(unix))]
fn keep_permissions(_temp: &NamedTempFile, _path: &Path) -> io::Result<()> {
    Ok(())
}

/// Flush a directory's entries (a completed rename) to stable storage.
/// Filesystems that cannot flush directories are tolerated, as for files.
#[cfg(unix)]
fn sync_dir(dir: &Path) -> io::Result<()> {
    let handle = match std::fs::File::open(dir) {
        Ok(h) => h,
        Err(e) => {
            warn!(dir = %dir.display(), error = %e, "Cannot open the directory to flush it; continuing without fsync");
            return Ok(());
        }
    };
    match handle.sync_all() {
        Err(e) if is_unsupported(&e) || e.raw_os_error() == Some(libc::EINVAL) => {
            warn!(dir = %dir.display(), "Filesystem cannot flush directories; continuing without fsync");
            Ok(())
        }
        other => other,
    }
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
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

    fn write_via(path: &Path, bytes: &[u8], overwrite: bool) -> io::Result<()> {
        let mut temp = temp_file_for(path)?;
        temp.write_all(bytes)?;
        persist(temp, path, overwrite)
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o7777
    }

    #[cfg(unix)]
    #[test]
    fn new_files_get_the_umask_mode_of_a_plain_create() {
        let dir = tempfile::tempdir().expect("tempdir");
        // `File::create` asks for 0666 and the kernel applies the umask.
        let reference = dir.path().join("reference");
        std::fs::File::create(&reference).expect("create");
        let path = dir.path().join("out.bin");
        write_via(&path, b"x", false).expect("write");
        assert_eq!(mode(&path), mode(&reference));
        assert_ne!(mode(&path) & 0o044, 0, "not tempfile's owner-only 0600");
    }

    #[cfg(unix)]
    #[test]
    fn overwriting_keeps_the_existing_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("out.bin");
        for wanted in [0o644, 0o640, 0o600, 0o755] {
            std::fs::write(&path, b"old").expect("seed");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(wanted))
                .expect("chmod");
            write_via(&path, b"new", true).expect("overwrite");
            assert_eq!(mode(&path), wanted, "mode {wanted:o}");
            assert_eq!(std::fs::read(&path).expect("read"), b"new");
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_destinations_replace_the_target_and_keep_the_link() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().expect("tempdir");
        let (links, data) = (dir.path().join("links"), dir.path().join("data"));
        std::fs::create_dir(&links).expect("mkdir");
        std::fs::create_dir(&data).expect("mkdir");
        let target = data.join("real.pbf");
        std::fs::write(&target, b"old").expect("seed");
        // A relative link, reached through a second (absolute) link.
        let link = links.join("current.pbf");
        symlink("../data/real.pbf", &link).expect("symlink");
        let outer = links.join("alias.pbf");
        symlink(&link, &outer).expect("symlink");
        assert_eq!(
            resolve_destination(&outer).expect("resolve"),
            links.join("../data/real.pbf")
        );
        assert_eq!(temp_dir_for(&outer).expect("dir"), links.join("../data"));

        let err = write_via(&outer, b"new", false).expect_err("target exists");
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        write_via(&outer, b"new", true).expect("overwrite");
        assert!(
            std::fs::symlink_metadata(&link)
                .expect("link")
                .file_type()
                .is_symlink()
        );
        assert!(
            std::fs::symlink_metadata(&outer)
                .expect("link")
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read(&target).expect("read"), b"new");
        assert_eq!(names(&links), ["alias.pbf", "current.pbf"]);
        assert_eq!(names(&data), ["real.pbf"], "no temporary left behind");

        // A dangling link is written through, creating its target.
        let dangling = links.join("next.pbf");
        symlink("../data/next.pbf", &dangling).expect("symlink");
        write_via(&dangling, b"fresh", false).expect("create target");
        assert_eq!(
            std::fs::read(data.join("next.pbf")).expect("read"),
            b"fresh"
        );
        assert!(
            std::fs::symlink_metadata(&dangling)
                .expect("link")
                .file_type()
                .is_symlink()
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_loops_are_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        std::os::unix::fs::symlink(&b, &a).expect("symlink");
        std::os::unix::fs::symlink(&a, &b).expect("symlink");
        assert!(write_via(&a, b"x", true).is_err());
        assert_eq!(names(dir.path()), ["a", "b"]);
    }

    #[test]
    fn bare_file_names_use_the_current_directory() {
        assert_eq!(
            temp_dir_for(Path::new("out.pbf")).expect("dir"),
            Path::new(".")
        );
    }

    #[cfg(unix)]
    #[test]
    fn enotsup_is_unsupported() {
        assert!(is_unsupported(&io::Error::from_raw_os_error(libc::ENOTSUP)));
        assert!(!is_unsupported(&io::Error::from_raw_os_error(libc::EACCES)));
    }
}
