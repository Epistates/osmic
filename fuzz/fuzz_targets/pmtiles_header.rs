//! Untrusted PMTiles headers: validation must never panic, and anything it
//! accepts must keep every section inside the file.

#![no_main]

use libfuzzer_sys::fuzz_target;
use osmic_tiles::reader::validate_header;

fuzz_target!(|input: (u64, Vec<u8>)| {
    let (file_len, header) = input;
    if validate_header(&header, file_len).is_ok() {
        let field = |at: usize| u64::from_le_bytes(header[at..at + 8].try_into().unwrap());
        for at in [8, 24, 40, 56] {
            assert!(field(at).checked_add(field(at + 8)).is_some_and(|end| end <= file_len));
        }
        assert!(field(8) >= 127);
    }
});
