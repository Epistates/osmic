//! Untrusted MapLibre style documents.

#![no_main]

use libfuzzer_sys::fuzz_target;
use osmic_style::Style;

fuzz_target!(|data: &[u8]| {
    if let Ok(text) = std::str::from_utf8(data)
        && let Ok(style) = Style::from_json(text)
    {
        // Whatever parses must serialise and parse back to the same model.
        let again = Style::from_json(&style.to_json()).expect("round trip");
        assert_eq!(again, style);
    }
});
