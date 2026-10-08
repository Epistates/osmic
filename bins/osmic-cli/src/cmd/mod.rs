pub mod extract;
pub mod inspect;
pub mod serve;
pub mod tiles;
pub mod update;

use std::io::Write;
use std::path::Path;

use anyhow::{Context, bail};

/// Fail early (before hours of work) if `path` exists and `force` is off,
/// or if its directory does not exist.
pub fn check_output(path: &Path, force: bool) -> anyhow::Result<()> {
    if path.exists() && !force {
        bail!(
            "{} already exists (pass --force to overwrite)",
            path.display()
        );
    }
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    if !dir.is_dir() {
        bail!("output directory {} does not exist", dir.display());
    }
    crate::cleanup::register_dir(dir);
    Ok(())
}

/// Write a small file atomically (temp file + rename).
pub fn write_file(path: &Path, bytes: &[u8], force: bool) -> anyhow::Result<()> {
    check_output(path, force)?;
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let mut temp = tempfile::Builder::new()
        .prefix(&osmic_core::fs::temp_file_prefix())
        .tempfile_in(dir)
        .with_context(|| format!("creating a temporary file in {}", dir.display()))?;
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    if force {
        temp.persist(path)?;
    } else {
        temp.persist_noclobber(path)?;
    }
    Ok(())
}

/// Thousands separators for counts.
pub fn fmt_count(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

pub fn fmt_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatting() {
        assert_eq!(fmt_count(0), "0");
        assert_eq!(fmt_count(1234567), "1,234,567");
        assert_eq!(fmt_bytes(512), "512 B");
        assert_eq!(fmt_bytes(3 << 20), "3.0 MiB");
    }

    #[test]
    fn output_checks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("a.txt");
        write_file(&p, b"one", false).expect("write");
        assert!(write_file(&p, b"two", false).is_err());
        write_file(&p, b"two", true).expect("force");
        assert_eq!(std::fs::read(&p).expect("read"), b"two");
        assert!(check_output(&dir.path().join("missing/x"), false).is_err());
    }
}
