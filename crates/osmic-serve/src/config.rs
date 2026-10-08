//! Server configuration.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use axum::http::HeaderValue;

use crate::error::ServeError;

/// Configuration for a [`TileServer`](crate::TileServer).
///
/// The struct is `#[non_exhaustive]`: build it with [`TileServerConfig::new`]
/// (or [`Default`]) and the chainable setters, so new options can be added
/// without breaking callers.
///
/// ```
/// use std::time::Duration;
/// use osmic_serve::TileServerConfig;
///
/// let config = TileServerConfig::new("tiles.pmtiles")
///     .bind_addr(([0, 0, 0, 0], 8080).into())
///     .public_url("https://tiles.example.com")
///     .cors_allowed_origins(["https://app.example.com"])
///     .request_timeout(Duration::from_secs(10))
///     .max_concurrency(256);
/// assert_eq!(config.max_concurrency, 256);
/// ```
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct TileServerConfig {
    /// Socket address used by [`TileServer::serve`](crate::TileServer::serve).
    pub bind_addr: SocketAddr,
    /// Path of the PMTiles archive.
    pub pmtiles_path: PathBuf,
    /// `max-age` (seconds) for tile responses.
    pub cache_max_age: u32,
    /// Externally visible base URL (for example `https://tiles.example.com`),
    /// used to build absolute URLs in `/style.json` and `/tiles.json`.
    /// When `None`, URLs are derived from the validated `Host` header.
    pub public_url: Option<String>,
    /// Allowed CORS origins. Empty means any origin (`*`).
    pub cors_allowed_origins: Vec<String>,
    /// Maximum time a request may take before it is answered with `408`.
    pub request_timeout: Duration,
    /// Maximum number of requests processed concurrently; further requests
    /// are shed with `503`.
    pub max_concurrency: usize,
    /// Maximum number of open client connections; further connections wait
    /// to be accepted.
    pub max_connections: usize,
    /// Time a client has to send a request's headers, also while an idle
    /// keep-alive connection waits for its next request.
    pub header_read_timeout: Duration,
    /// After a shutdown signal, keep serving this long with `/readyz`
    /// failing so load balancers stop routing here before connections close
    /// (5–10 s behind Kubernetes Services; 0 to stop accepting at once).
    pub drain_delay: Duration,
    /// Longest wait for in-flight requests after accepting stops; remaining
    /// connections are then closed.
    pub drain_timeout: Duration,
    /// Decoded leaf directories of the archive kept in memory.
    pub directory_cache: u64,
}

impl Default for TileServerConfig {
    fn default() -> Self {
        Self {
            bind_addr: ([127, 0, 0, 1], 3000).into(),
            pmtiles_path: PathBuf::from("tiles.pmtiles"),
            cache_max_age: 3600,
            public_url: None,
            cors_allowed_origins: Vec::new(),
            request_timeout: Duration::from_secs(30),
            max_concurrency: 1024,
            max_connections: 4096,
            header_read_timeout: Duration::from_secs(10),
            drain_delay: Duration::ZERO,
            drain_timeout: Duration::from_secs(30),
            directory_cache: osmic_tiles::reader::DEFAULT_DIRECTORY_CACHE,
        }
    }
}

impl TileServerConfig {
    /// Create a configuration for the given archive with default settings
    /// (loopback port 3000, one hour cache, 30 s timeout, 1024 concurrent requests).
    pub fn new(pmtiles_path: impl Into<PathBuf>) -> Self {
        Self {
            pmtiles_path: pmtiles_path.into(),
            ..Self::default()
        }
    }

    /// Set the listen address used by [`TileServer::serve`](crate::TileServer::serve).
    #[must_use]
    pub fn bind_addr(mut self, addr: SocketAddr) -> Self {
        self.bind_addr = addr;
        self
    }

    /// Set the PMTiles archive path.
    #[must_use]
    pub fn pmtiles_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.pmtiles_path = path.into();
        self
    }

    /// Set the `max-age` (seconds) of tile responses.
    #[must_use]
    pub fn cache_max_age(mut self, seconds: u32) -> Self {
        self.cache_max_age = seconds;
        self
    }

    /// Set the externally visible base URL (scheme, host, optional port and path prefix).
    #[must_use]
    pub fn public_url(mut self, url: impl Into<String>) -> Self {
        self.public_url = Some(url.into());
        self
    }

    /// Restrict CORS to the given origins (for example `https://app.example.com`).
    /// An empty list allows any origin.
    #[must_use]
    pub fn cors_allowed_origins<I, S>(mut self, origins: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.cors_allowed_origins = origins.into_iter().map(Into::into).collect();
        self
    }

    /// Set the per-request timeout.
    #[must_use]
    pub fn request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// Set the maximum number of concurrently processed requests.
    #[must_use]
    pub fn max_concurrency(mut self, max: usize) -> Self {
        self.max_concurrency = max;
        self
    }

    /// Set the maximum number of open client connections.
    #[must_use]
    pub fn max_connections(mut self, max: usize) -> Self {
        self.max_connections = max;
        self
    }

    /// Set how long a client may take to send request headers.
    #[must_use]
    pub fn header_read_timeout(mut self, timeout: Duration) -> Self {
        self.header_read_timeout = timeout;
        self
    }

    /// Set how long to keep serving, with `/readyz` failing, after a
    /// shutdown signal.
    #[must_use]
    pub fn drain_delay(mut self, delay: Duration) -> Self {
        self.drain_delay = delay;
        self
    }

    /// Set the longest wait for in-flight requests during shutdown.
    #[must_use]
    pub fn drain_timeout(mut self, timeout: Duration) -> Self {
        self.drain_timeout = timeout;
        self
    }

    /// Set how many decoded leaf directories to cache.
    #[must_use]
    pub fn directory_cache(mut self, directories: u64) -> Self {
        self.directory_cache = directories;
        self
    }

    /// Validate the configuration, returning the normalized public URL
    /// (no trailing slash) and parsed CORS origins.
    pub(crate) fn validate(&self) -> Result<ValidatedConfig, ServeError> {
        if self.max_concurrency == 0 {
            return Err(invalid("max_concurrency", "must be at least 1"));
        }
        if self.request_timeout.is_zero() {
            return Err(invalid("request_timeout", "must be greater than zero"));
        }
        if self.max_connections == 0 {
            return Err(invalid("max_connections", "must be at least 1"));
        }
        if self.header_read_timeout.is_zero() {
            return Err(invalid("header_read_timeout", "must be greater than zero"));
        }
        if self.drain_timeout.is_zero() {
            return Err(invalid("drain_timeout", "must be greater than zero"));
        }
        if self.directory_cache == 0 {
            return Err(invalid("directory_cache", "must be at least 1"));
        }
        let public_url = self
            .public_url
            .as_deref()
            .map(normalize_public_url)
            .transpose()?;
        let cors_origins = self
            .cors_allowed_origins
            .iter()
            .map(|o| parse_origin(o))
            .collect::<Result<Vec<_>, _>>()?;
        let public_origin = public_url.as_deref().map(origin_of);
        Ok(ValidatedConfig {
            public_url,
            public_origin,
            cors_origins,
        })
    }
}

pub(crate) struct ValidatedConfig {
    pub public_url: Option<String>,
    /// `scheme://authority` of `public_url`.
    pub public_origin: Option<String>,
    pub cors_origins: Vec<HeaderValue>,
}

/// `scheme://authority` of a normalized public URL.
fn origin_of(url: &str) -> String {
    let after_scheme = url.find("://").map_or(0, |i| i + 3);
    match url[after_scheme..].find('/') {
        Some(i) => url[..after_scheme + i].to_string(),
        None => url.to_string(),
    }
}

fn invalid(field: &'static str, reason: impl Into<String>) -> ServeError {
    ServeError::InvalidConfig {
        field,
        reason: reason.into(),
    }
}

/// Accept only `http(s)://authority[/path]` built from conservative characters,
/// so the value can be embedded in JSON and HTML verbatim.
fn normalize_public_url(raw: &str) -> Result<String, ServeError> {
    let trimmed = raw.trim_end_matches('/');
    let rest = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))
        .ok_or_else(|| invalid("public_url", "must start with http:// or https://"))?;
    let (authority, path) = match rest.find('/') {
        Some(i) => rest.split_at(i),
        None => (rest, ""),
    };
    if !crate::http::is_valid_authority(authority) {
        return Err(invalid("public_url", "invalid host[:port]"));
    }
    let path_ok = path
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'.' | b'_' | b'~'));
    if !path_ok || path.contains("//") {
        return Err(invalid(
            "public_url",
            "path contains unsupported characters",
        ));
    }
    Ok(trimmed.to_owned())
}

fn parse_origin(raw: &str) -> Result<HeaderValue, ServeError> {
    let origin = raw.trim_end_matches('/');
    let rest = origin
        .strip_prefix("https://")
        .or_else(|| origin.strip_prefix("http://"))
        .ok_or_else(|| invalid("cors_allowed_origins", format!("`{raw}` is not an origin")))?;
    if !crate::http::is_valid_authority(rest) {
        return Err(invalid(
            "cors_allowed_origins",
            format!("`{raw}` is not an origin"),
        ));
    }
    HeaderValue::from_str(origin)
        .map_err(|_| invalid("cors_allowed_origins", format!("`{raw}` is not an origin")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_loopback_and_sane() {
        let c = TileServerConfig::default();
        assert!(c.bind_addr.ip().is_loopback());
        assert_eq!(c.bind_addr.port(), 3000);
        assert_eq!(c.cache_max_age, 3600);
        assert!(c.public_url.is_none());
        assert!(c.cors_allowed_origins.is_empty());
    }

    #[test]
    fn public_url_is_normalized_and_validated() {
        let ok = TileServerConfig::new("x").public_url("https://a.example.com:8443/base/");
        assert_eq!(
            ok.validate().unwrap().public_url.as_deref(),
            Some("https://a.example.com:8443/base")
        );
        for bad in [
            "ftp://x",
            "https://",
            "https://a\"b",
            "https://a.com/?q=1",
            "a.com",
        ] {
            let c = TileServerConfig::new("x").public_url(bad);
            assert!(c.validate().is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn public_origin_drops_the_path() {
        let v = TileServerConfig::new("x")
            .public_url("https://a.example.com:8443/base/")
            .validate()
            .unwrap();
        assert_eq!(
            v.public_origin.as_deref(),
            Some("https://a.example.com:8443")
        );
        assert_eq!(origin_of("http://h"), "http://h");
    }

    #[test]
    fn connection_limits_are_validated() {
        for (c, field) in [
            (
                TileServerConfig::new("x").max_connections(0),
                "max_connections",
            ),
            (
                TileServerConfig::new("x").header_read_timeout(Duration::ZERO),
                "header_read_timeout",
            ),
            (
                TileServerConfig::new("x").drain_timeout(Duration::ZERO),
                "drain_timeout",
            ),
            (
                TileServerConfig::new("x").directory_cache(0),
                "directory_cache",
            ),
        ] {
            assert!(
                matches!(c.validate(), Err(ServeError::InvalidConfig { field: f, .. }) if f == field),
                "{field}"
            );
        }
    }

    #[test]
    fn zero_concurrency_and_bad_origin_rejected() {
        assert!(
            TileServerConfig::new("x")
                .max_concurrency(0)
                .validate()
                .is_err()
        );
        assert!(
            TileServerConfig::new("x")
                .cors_allowed_origins(["not-an-origin"])
                .validate()
                .is_err()
        );
    }
}
