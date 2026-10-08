//! Small, dependency-free HTTP helpers: content negotiation, validators and
//! `Host` validation.

use axum::http::{HeaderMap, header};

/// Cache policy for non-tile, effectively static documents (style, TileJSON,
/// metadata, viewer assets).
pub(crate) const SHORT_CACHE: &str = "public, max-age=60";
/// Cache policy for errors and health probes.
pub(crate) const NO_STORE: &str = "no-store";

/// Returns `true` if the request's `Accept-Encoding` permits `coding`
/// (for example `"gzip"`). An explicit entry wins over `*`; `q=0` forbids.
pub(crate) fn accepts_encoding(headers: &HeaderMap, coding: &str) -> bool {
    let mut explicit: Option<bool> = None;
    let mut wildcard: Option<bool> = None;
    for value in headers.get_all(header::ACCEPT_ENCODING) {
        let Ok(value) = value.to_str() else { continue };
        for item in value.split(',') {
            let mut parts = item.split(';');
            let token = parts.next().unwrap_or("").trim();
            let allowed = parts
                .filter_map(|p| {
                    p.trim()
                        .strip_prefix("q=")
                        .or_else(|| p.trim().strip_prefix("Q="))
                })
                .next()
                .map(|q| q.trim().parse::<f32>().is_ok_and(|q| q > 0.0))
                .unwrap_or(true);
            if token.eq_ignore_ascii_case(coding) {
                explicit = Some(allowed);
            } else if token == "*" {
                wildcard = Some(allowed);
            }
        }
    }
    explicit.or(wildcard).unwrap_or(false)
}

/// Evaluates `If-None-Match` against `etag` using weak comparison (RFC 9110 §13.1.2).
pub(crate) fn if_none_match_hits(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .any(|candidate| {
            candidate == "*" || candidate.strip_prefix("W/").unwrap_or(candidate) == etag
        })
}

/// Validates `host[:port]` (reg-name, IPv4 or bracketed IPv6) using a strict
/// character whitelist. Rejects userinfo, paths, whitespace and quotes.
pub(crate) fn is_valid_authority(s: &str) -> bool {
    if s.is_empty() || s.len() > 255 {
        return false;
    }
    let (host, port) = if let Some(rest) = s.strip_prefix('[') {
        let Some(end) = rest.find(']') else {
            return false;
        };
        let (inner, after) = rest.split_at(end);
        if inner.is_empty()
            || !inner
                .bytes()
                .all(|b| b.is_ascii_hexdigit() || b == b':' || b == b'.')
        {
            return false;
        }
        match after[1..].strip_prefix(':') {
            Some(p) => ("v6", Some(p)),
            None if after.len() == 1 => ("v6", None),
            None => return false,
        }
    } else {
        match s.split_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (s, None),
        }
    };
    if host != "v6"
        && (host.is_empty()
            || host.starts_with(['.', '-'])
            || !host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_')))
    {
        return false;
    }
    port.is_none_or(|p| (1..=5).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderName, HeaderValue};

    fn h(name: HeaderName, v: &str) -> HeaderMap {
        let mut m = HeaderMap::new();
        m.insert(name, HeaderValue::from_str(v).unwrap());
        m
    }

    #[test]
    fn accept_encoding_parsing() {
        let ae = |v: &str| accepts_encoding(&h(header::ACCEPT_ENCODING, v), "gzip");
        assert!(ae("gzip"));
        assert!(ae("br, GZIP;q=0.5"));
        assert!(ae("*"));
        assert!(!ae("gzip;q=0"));
        assert!(!ae("*, gzip;q=0"));
        assert!(!ae("identity"));
        assert!(!accepts_encoding(&HeaderMap::new(), "gzip"));
    }

    #[test]
    fn if_none_match_forms() {
        let m = |v: &str| if_none_match_hits(&h(header::IF_NONE_MATCH, v), "\"abc\"");
        assert!(m("\"abc\""));
        assert!(m("W/\"abc\""));
        assert!(m("\"x\", \"abc\""));
        assert!(m("*"));
        assert!(!m("\"abd\""));
    }

    #[test]
    fn authority_whitelist() {
        for ok in [
            "localhost",
            "a.b-c.example:8080",
            "127.0.0.1:3000",
            "[::1]:3000",
            "[::1]",
        ] {
            assert!(is_valid_authority(ok), "{ok}");
        }
        for bad in [
            "",
            "a b",
            "a/b",
            "a@b",
            "a\"b",
            "a.com:",
            "a.com:99999x",
            "<script>",
            "a:b:c",
            "-a",
            "[::1",
            "[]",
            "a.com\r\nX: y",
        ] {
            assert!(!is_valid_authority(bad), "{bad}");
        }
    }
}
