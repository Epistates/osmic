//! Untrusted vector tiles: decoding must never panic, loop or overflow.

#![no_main]

use libfuzzer_sys::fuzz_target;
use osmic_core::TileCoord;
use osmic_tiles::mvt_decode::{decode_layers, decode_tile};

fuzz_target!(|data: &[u8]| {
    let _ = decode_layers(data);
    if let Some(tile) = TileCoord::try_new(1, 2, 2) {
        let _ = decode_tile(data, tile);
    }
});
