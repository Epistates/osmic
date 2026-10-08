//! Replication state: a sequence number, its timestamp and the server.
//!
//! The canonical copy lives in the PBF header (`osmosis_replication_*`
//! fields, as written by osmium and pyosmium), so data and state are always
//! updated together. Servers publish the same information as `state.txt`
//! (Java properties format).

use crate::error::ReplError;

/// Where a dataset stands in a replication stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplicationState {
    /// Sequence number of the last diff the data includes.
    pub sequence: u64,
    /// ISO-8601 timestamp of the sequence (`2026-10-08T12:00:00Z`), if known.
    pub timestamp: Option<String>,
    /// Replication base URL (directory containing `state.txt`).
    pub base_url: String,
}

/// Replication directory path for a sequence: `000/123/456`.
///
/// # Errors
///
/// [`ReplError::State`] if `sequence` has more than nine digits.
pub fn sequence_path(sequence: u64) -> Result<String, ReplError> {
    if sequence > 999_999_999 {
        return Err(ReplError::State(format!(
            "sequence {sequence} exceeds 9 digits"
        )));
    }
    Ok(format!(
        "{:03}/{:03}/{:03}",
        sequence / 1_000_000,
        (sequence / 1_000) % 1_000,
        sequence % 1_000
    ))
}

impl ReplicationState {
    /// URL of the diff that follows this state.
    ///
    /// # Errors
    ///
    /// [`ReplError::State`] if the next sequence overflows or has more than
    /// nine digits.
    pub fn next_diff_url(&self) -> Result<String, ReplError> {
        let next = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| ReplError::State("sequence overflow".into()))?;
        Ok(format!("{}/{}.osc.gz", self.base(), sequence_path(next)?))
    }

    fn base(&self) -> &str {
        self.base_url.trim_end_matches('/')
    }

    /// Parse a server `state.txt`, recording `base_url` as its source.
    ///
    /// Unknown keys are ignored. The timestamp is kept in the canonical
    /// `YYYY-MM-DDTHH:MM:SSZ` form; one that is not a valid instant is
    /// dropped.
    ///
    /// # Errors
    ///
    /// [`ReplError::State`] if `sequenceNumber` is missing or not an
    /// unsigned integer.
    pub fn parse_state_txt(text: &str, base_url: &str) -> Result<Self, ReplError> {
        let mut sequence = None;
        let mut timestamp = None;
        for line in text.lines().map(str::trim) {
            if line.starts_with('#') || line.is_empty() {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            match k.trim() {
                "sequenceNumber" => {
                    sequence =
                        Some(v.trim().parse::<u64>().map_err(|_| {
                            ReplError::State(format!("invalid sequenceNumber {v:?}"))
                        })?);
                }
                // Properties files escape ':' as '\:'. A timestamp that is
                // not a valid instant is dropped rather than trusted.
                "timestamp" => {
                    timestamp =
                        parse_iso8601(&v.trim().replace("\\:", ":")).and_then(format_iso8601);
                }
                _ => {}
            }
        }
        Ok(Self {
            sequence: sequence
                .ok_or_else(|| ReplError::State("state.txt without sequenceNumber".into()))?,
            timestamp,
            base_url: base_url.to_string(),
        })
    }

    /// Render as `state.txt`.
    pub fn to_state_txt(&self) -> String {
        let mut s = format!("sequenceNumber={}\n", self.sequence);
        if let Some(t) = &self.timestamp {
            s.push_str(&format!("timestamp={}\n", t.replace(':', "\\:")));
        }
        s
    }

    /// Seconds since the Unix epoch for the state's timestamp.
    pub fn unix_timestamp(&self) -> Option<i64> {
        parse_iso8601(self.timestamp.as_deref()?)
    }
}

/// Seconds since the Unix epoch for an RFC 3339 instant such as
/// `2026-10-08T12:00:00Z` (the form OSM replication uses), or `None` if `s`
/// is not one. Fractional seconds are truncated.
pub fn parse_iso8601(s: &str) -> Option<i64> {
    s.parse::<jiff::Timestamp>().ok().map(|t| t.as_second())
}

/// `unix` as `YYYY-MM-DDTHH:MM:SSZ`, the inverse of [`parse_iso8601`], or
/// `None` outside the years -9999..=9999.
pub fn format_iso8601(unix: i64) -> Option<String> {
    jiff::Timestamp::from_second(unix)
        .ok()
        .map(|t| t.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequence_paths() {
        assert_eq!(sequence_path(0).expect("ok"), "000/000/000");
        assert_eq!(sequence_path(6_123_456).expect("ok"), "006/123/456");
        assert!(sequence_path(1_000_000_000).is_err());
        let s = ReplicationState {
            sequence: 4_999_999,
            timestamp: None,
            base_url: "https://example.org/replication/minute/".into(),
        };
        assert_eq!(
            s.next_diff_url().expect("ok"),
            "https://example.org/replication/minute/005/000/000.osc.gz"
        );
    }

    #[test]
    fn state_txt_round_trip() {
        let text = "#Wed Oct 08 12:00:02 UTC 2026\nsequenceNumber=6123456\ntimestamp=2026-10-08T12\\:00\\:00Z\n";
        let s = ReplicationState::parse_state_txt(text, "https://x").expect("valid");
        assert_eq!(s.sequence, 6_123_456);
        assert_eq!(s.timestamp.as_deref(), Some("2026-10-08T12:00:00Z"));
        assert_eq!(
            ReplicationState::parse_state_txt(&s.to_state_txt(), "https://x").expect("ok"),
            s
        );
        assert!(ReplicationState::parse_state_txt("timestamp=x", "u").is_err());
        assert!(ReplicationState::parse_state_txt("sequenceNumber=-1", "u").is_err());
        // Garbage timestamps (including control characters) are dropped.
        let s =
            ReplicationState::parse_state_txt("sequenceNumber=5\ntimestamp=\u{1b}[31mnow\n", "u")
                .expect("valid");
        assert_eq!(s.timestamp, None);
    }

    #[test]
    fn iso8601_round_trip() {
        for t in [
            "1970-01-01T00:00:00Z",
            "2000-02-29T23:59:59Z",
            "2026-10-08T12:00:00Z",
        ] {
            let unix = parse_iso8601(t).expect(t);
            assert_eq!(format_iso8601(unix).as_deref(), Some(t));
        }
        // Any explicit offset is an instant; state files keep the Z form.
        assert_eq!(
            parse_iso8601("2026-10-08T14:00:00+02:00"),
            Some(1_791_460_800)
        );
        let s = ReplicationState::parse_state_txt(
            "sequenceNumber=5\ntimestamp=2026-10-08T14\\:00\\:00+02\\:00\n",
            "u",
        )
        .expect("valid");
        assert_eq!(s.timestamp.as_deref(), Some("2026-10-08T12:00:00Z"));
        // Header timestamps beyond the representable range are unknown,
        // not formatted into a string that cannot be parsed back.
        assert_eq!(format_iso8601(i64::MAX), None);
        assert_eq!(parse_iso8601("2026-10-08T12:00:00Z"), Some(1_791_460_800));
        assert_eq!(parse_iso8601("2026-13-08T12:00:00Z"), None);
        assert_eq!(parse_iso8601("2026-02-31T12:00:00Z"), None);
        assert_eq!(parse_iso8601("2026-02-29T12:00:00Z"), None);
        assert!(parse_iso8601("2028-02-29T12:00:00Z").is_some());
        assert_eq!(parse_iso8601("2026-04-31T00:00:00Z"), None);
        assert_eq!(parse_iso8601("garbage"), None);
    }
}
