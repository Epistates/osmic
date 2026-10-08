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
    pub sequence: u64,
    /// ISO-8601 timestamp of the sequence (`2026-10-08T12:00:00Z`), if known.
    pub timestamp: Option<String>,
    /// Replication base URL (directory containing `state.txt`).
    pub base_url: String,
}

/// Replication directory path for a sequence: `000/123/456`.
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

    /// Parse a server `state.txt`.
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
                    let t = v.trim().replace("\\:", ":");
                    timestamp = parse_iso8601(&t).is_some().then_some(t);
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

/// Parse `YYYY-MM-DDTHH:MM:SSZ` (the only form OSM replication uses).
pub fn parse_iso8601(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() != 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[19] != b'Z' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, m, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hh, mm, ss) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let days_in_month = match m {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        1..=12 => 31,
        _ => return None,
    };
    if !(1..=days_in_month).contains(&d) || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    // Days from civil (Howard Hinnant's algorithm).
    let (y, m) = if m <= 2 { (y - 1, m + 9) } else { (y, m - 3) };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * m + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hh * 3_600 + mm * 60 + ss)
}

/// Inverse of [`parse_iso8601`].
pub fn format_iso8601(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        secs / 3_600,
        (secs / 60) % 60,
        secs % 60
    )
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
            assert_eq!(format_iso8601(unix), t);
        }
        assert_eq!(parse_iso8601("2026-10-08T12:00:00Z"), Some(1_791_460_800));
        assert_eq!(parse_iso8601("2026-13-08T12:00:00Z"), None);
        assert_eq!(parse_iso8601("2026-02-31T12:00:00Z"), None);
        assert_eq!(parse_iso8601("2026-02-29T12:00:00Z"), None);
        assert!(parse_iso8601("2028-02-29T12:00:00Z").is_some());
        assert_eq!(parse_iso8601("2026-04-31T00:00:00Z"), None);
        assert_eq!(parse_iso8601("garbage"), None);
    }
}
