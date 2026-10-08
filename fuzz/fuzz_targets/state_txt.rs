//! Untrusted replication `state.txt` documents and timestamps.

#![no_main]

use libfuzzer_sys::fuzz_target;
use osmic_repl::ReplicationState;
use osmic_repl::state::{format_iso8601, parse_iso8601};

fuzz_target!(|data: &[u8]| {
    if let Ok(text) = std::str::from_utf8(data) {
        if let Ok(state) = ReplicationState::parse_state_txt(text, "https://example.org") {
            let again = ReplicationState::parse_state_txt(&state.to_state_txt(), "https://example.org");
            assert_eq!(again.ok().as_ref(), Some(&state));
        }
        if let Some(unix) = parse_iso8601(text) {
            let formatted = format_iso8601(unix).expect("a parsed instant is in range");
            assert_eq!(parse_iso8601(&formatted), Some(unix));
        }
    }
});
