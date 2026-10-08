//! Build script: compiles the Metal compute shaders in `src/kernels/` into a
//! `.metallib` that is embedded into the crate.
//!
//! The Metal backend is strictly optional. This script emits
//! `cargo::rustc-cfg=osmic_metallib` **only** when a metallib was actually
//! produced. In every other situation (non-macOS target, `DOCS_RS`, missing
//! Metal toolchain, shader compile failure, or `OSMIC_ACCEL_SKIP_SHADERS`) the
//! crate still builds; it simply reports the GPU as unavailable at runtime.
//!
//! Environment knobs:
//! - `OSMIC_ACCEL_SKIP_SHADERS=1`: skip shader compilation (useful to test the
//!   "no metallib" configuration on a machine that has the toolchain).
//! - `MACOSX_DEPLOYMENT_TARGET`: minimum macOS version the metallib is built
//!   for (default [`DEFAULT_DEPLOYMENT_TARGET`]). The shaders use no features
//!   beyond Metal 2.x, so any value from 11.0 upwards works.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Default minimum macOS version for the metallib. Matches Rust's default
/// `aarch64-apple-darwin` deployment target (11.0) so the metallib never
/// requires a newer OS than the Rust binary itself.
const DEFAULT_DEPLOYMENT_TARGET: &str = "11.0";

const METALLIB_NAME: &str = "osmic_geometry.metallib";

fn main() {
    println!("cargo::rustc-check-cfg=cfg(osmic_metallib)");
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-changed=src/kernels");
    println!("cargo::rerun-if-env-changed=DOCS_RS");
    println!("cargo::rerun-if-env-changed=OSMIC_ACCEL_SKIP_SHADERS");
    println!("cargo::rerun-if-env-changed=MACOSX_DEPLOYMENT_TARGET");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    if env::var_os("DOCS_RS").is_some() || env::var_os("OSMIC_ACCEL_SKIP_SHADERS").is_some() {
        return;
    }

    match compile_shaders() {
        Ok(()) => println!("cargo::rustc-cfg=osmic_metallib"),
        Err(reason) => println!(
            "cargo::warning=osmic-accel: GPU backend disabled ({reason}); \
             the CPU reference path remains available"
        ),
    }
}

fn compile_shaders() -> Result<(), String> {
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").ok_or("OUT_DIR is not set")?);
    let manifest_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").ok_or("CARGO_MANIFEST_DIR is not set")?);
    let shaders_dir = manifest_dir.join("src").join("kernels");

    let found = Command::new("xcrun")
        .args(["--sdk", "macosx", "--find", "metal"])
        .output()
        .map_err(|e| format!("cannot run xcrun: {e}"))?;
    if !found.status.success() {
        return Err("Metal compiler not found; install Xcode or its Metal Toolchain".into());
    }

    let mut sources: Vec<PathBuf> = fs::read_dir(&shaders_dir)
        .map_err(|e| format!("cannot read {}: {e}", shaders_dir.display()))?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|ext| ext == "metal"))
        .collect();
    sources.sort();
    if sources.is_empty() {
        return Err(format!("no .metal files in {}", shaders_dir.display()));
    }

    let deployment_target = env::var("MACOSX_DEPLOYMENT_TARGET")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_DEPLOYMENT_TARGET.to_string());

    let mut air_files = Vec::with_capacity(sources.len());
    for source in &sources {
        println!("cargo::rerun-if-changed={}", source.display());
        let stem = source
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| format!("bad shader file name {}", source.display()))?;
        let air = out_dir.join(format!("{stem}.air"));

        let output = Command::new("xcrun")
            .args(["-sdk", "macosx", "metal", "-O3", "-ffast-math"])
            .arg(format!("-mmacosx-version-min={deployment_target}"))
            .arg("-c")
            .arg(source)
            .arg("-o")
            .arg(&air)
            .output()
            .map_err(|e| format!("cannot run the Metal compiler: {e}"))?;
        if !output.status.success() {
            return Err(format!(
                "compiling {} failed: {}",
                source.display(),
                summarize(&output.stderr)
            ));
        }
        air_files.push(air);
    }

    link_metallib(&air_files, &out_dir.join(METALLIB_NAME))
}

fn link_metallib(air_files: &[PathBuf], metallib: &Path) -> Result<(), String> {
    let output = Command::new("xcrun")
        .args(["-sdk", "macosx", "metallib"])
        .args(air_files)
        .arg("-o")
        .arg(metallib)
        .output()
        .map_err(|e| format!("cannot run the metallib linker: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "linking the metallib failed: {}",
            summarize(&output.stderr)
        ))
    }
}

/// First few lines of a tool's stderr, flattened so it fits in a single
/// `cargo::warning` line.
fn summarize(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .take(3)
        .collect::<Vec<_>>()
        .join(" | ")
}
