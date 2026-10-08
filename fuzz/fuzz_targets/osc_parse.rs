//! Untrusted OSM change files, plain and gzipped (size-capped).

#![no_main]

use std::io::Write;

use flate2::Compression;
use flate2::write::GzEncoder;
use libfuzzer_sys::fuzz_target;
use osmic_repl::{OscLimits, parse_osc, parse_osc_gz};

fuzz_target!(|data: &[u8]| {
    let _ = parse_osc(data);
    let mut limits = OscLimits::default();
    limits.max_decompressed_bytes = 1 << 20;
    let _ = parse_osc_gz(data, limits);
    let mut gz = GzEncoder::new(Vec::new(), Compression::fast());
    if gz.write_all(data).is_ok()
        && let Ok(compressed) = gz.finish()
    {
        let _ = parse_osc_gz(&compressed[..], OscLimits::default());
    }
});
