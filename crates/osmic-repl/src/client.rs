//! HTTP access to an OSM replication server.
//!
//! - HTTPS is required unless explicitly allowed (a man-in-the-middle could
//!   otherwise feed arbitrary edits into the dataset).
//! - Connect and overall timeouts bound every request.
//! - Response bodies are size-limited.
//! - Network errors and 5xx responses are retried with exponential backoff;
//!   a 404 for the next diff means "not published yet", not an error.

use std::thread::sleep;
use std::time::Duration;

use tracing::{debug, warn};

use crate::error::ReplError;
use crate::state::{ReplicationState, sequence_path};

/// Client settings.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ClientOptions {
    /// Accept `http://` base URLs and redirects; HTTPS only otherwise.
    pub allow_http: bool,
    /// Time limit for establishing a connection.
    pub connect_timeout: Duration,
    /// Overall time limit for one request, body included.
    pub request_timeout: Duration,
    /// Maximum size of one downloaded (compressed) diff.
    pub max_diff_bytes: u64,
    /// Attempts per request for transient failures.
    pub attempts: u32,
    /// Delay before the first retry; doubles each time.
    pub retry_delay: Duration,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            allow_http: false,
            connect_timeout: Duration::from_secs(15),
            request_timeout: Duration::from_secs(300),
            max_diff_bytes: 1 << 30,
            attempts: 4,
            retry_delay: Duration::from_secs(2),
        }
    }
}

impl ClientOptions {
    /// Allow plain-HTTP servers (not recommended).
    #[must_use]
    pub fn allow_http(mut self, allow: bool) -> Self {
        self.allow_http = allow;
        self
    }

    /// Overall time limit for one request.
    #[must_use]
    pub fn request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// Largest (compressed) diff accepted.
    #[must_use]
    pub fn max_diff_bytes(mut self, bytes: u64) -> Self {
        self.max_diff_bytes = bytes;
        self
    }

    /// Attempts per request for transient failures.
    #[must_use]
    pub fn attempts(mut self, attempts: u32) -> Self {
        self.attempts = attempts;
        self
    }
}

/// A replication server.
pub struct ReplicationClient {
    base_url: String,
    agent: ureq::Agent,
    options: ClientOptions,
}

enum Fetch {
    Found(Vec<u8>),
    NotFound,
}

impl ReplicationClient {
    /// A client for the replication directory at `base_url` (the directory
    /// holding `state.txt`; a trailing `/` is ignored). No request is made.
    ///
    /// # Errors
    ///
    /// [`ReplError::InsecureUrl`] if `base_url` is not `https://` (or `http://`
    /// with [`ClientOptions::allow_http`]).
    pub fn new(base_url: &str, options: ClientOptions) -> Result<Self, ReplError> {
        let lower = base_url.to_ascii_lowercase();
        if !(lower.starts_with("https://") || (options.allow_http && lower.starts_with("http://")))
        {
            return Err(ReplError::InsecureUrl {
                url: base_url.to_string(),
            });
        }
        let agent = ureq::Agent::config_builder()
            .https_only(!options.allow_http)
            .timeout_connect(Some(options.connect_timeout))
            .timeout_global(Some(options.request_timeout))
            .max_redirects(5)
            .http_status_as_error(false)
            .user_agent(concat!("osmic/", env!("CARGO_PKG_VERSION")))
            .build()
            .new_agent();
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            agent,
            options,
        })
    }

    /// The replication directory URL, without a trailing `/`.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    fn fetch(&self, url: &str, limit: u64) -> Result<Fetch, ReplError> {
        let mut delay = self.options.retry_delay;
        let attempts = self.options.attempts.max(1);
        for attempt in 1..=attempts {
            debug!(url, attempt, "GET");
            // Diffs are already gzip files; ask servers not to wrap them in a
            // transfer encoding as well (both forms are accepted anyway).
            let request = self.agent.get(url).header("Accept-Encoding", "identity");
            let outcome: Result<(), String> = match request.call() {
                Ok(mut resp) => {
                    let status = resp.status().as_u16();
                    match status {
                        200 => match resp.body_mut().with_config().limit(limit).read_to_vec() {
                            Ok(bytes) => return Ok(Fetch::Found(bytes)),
                            Err(ureq::Error::BodyExceedsLimit(_)) => {
                                return Err(ReplError::TooLarge {
                                    what: "download",
                                    limit,
                                });
                            }
                            Err(e) => Err(e.to_string()),
                        },
                        404 => return Ok(Fetch::NotFound),
                        500..=599 | 429 => Err(format!("HTTP {status}")),
                        _ => {
                            return Err(ReplError::Http {
                                url: url.to_string(),
                                message: format!("HTTP {status}"),
                            });
                        }
                    }
                }
                Err(e) => Err(e.to_string()),
            };
            if let Err(message) = outcome {
                if attempt == attempts {
                    return Err(ReplError::Http {
                        url: url.to_string(),
                        message,
                    });
                }
                warn!(url, attempt, error = %message, retry_in = ?delay, "replication request failed; retrying");
                sleep(delay);
                delay = delay.saturating_mul(2);
            }
        }
        unreachable!("loop returns on the last attempt")
    }

    /// The server's newest state.
    ///
    /// # Errors
    ///
    /// [`ReplError::Http`] if the request fails after retries, is rejected,
    /// or finds no `state.txt`; [`ReplError::TooLarge`] for a response over
    /// 64 KiB; [`ReplError::State`] if the file has no valid sequence number.
    pub fn latest_state(&self) -> Result<ReplicationState, ReplError> {
        let url = format!("{}/state.txt", self.base_url);
        match self.fetch(&url, 64 << 10)? {
            Fetch::Found(bytes) => {
                ReplicationState::parse_state_txt(&String::from_utf8_lossy(&bytes), &self.base_url)
            }
            Fetch::NotFound => Err(ReplError::Http {
                url,
                message: "no state.txt (is this a replication directory?)".into(),
            }),
        }
    }

    /// The state for a given sequence, if published.
    ///
    /// # Errors
    ///
    /// As for [`ReplicationClient::latest_state`], except that a missing
    /// file is `Ok(None)`; also [`ReplError::State`] for a sequence above
    /// 999 999 999.
    pub fn state(&self, sequence: u64) -> Result<Option<ReplicationState>, ReplError> {
        let url = format!("{}/{}.state.txt", self.base_url, sequence_path(sequence)?);
        match self.fetch(&url, 64 << 10)? {
            Fetch::Found(bytes) => Ok(Some(ReplicationState::parse_state_txt(
                &String::from_utf8_lossy(&bytes),
                &self.base_url,
            )?)),
            Fetch::NotFound => Ok(None),
        }
    }

    /// The gzipped diff for `sequence`, or `None` if not yet published.
    ///
    /// The body is not validated. It is usually gzip, but plain XML when the
    /// server added a gzip content encoding on top;
    /// [`parse_osc_auto_with`](crate::osc::parse_osc_auto_with) accepts
    /// either.
    ///
    /// # Errors
    ///
    /// [`ReplError::State`] for a sequence above 999 999 999;
    /// [`ReplError::Http`] if the request fails after retries or is
    /// rejected; [`ReplError::TooLarge`] for a body over
    /// [`ClientOptions::max_diff_bytes`].
    pub fn diff(&self, sequence: u64) -> Result<Option<Vec<u8>>, ReplError> {
        let url = format!("{}/{}.osc.gz", self.base_url, sequence_path(sequence)?);
        Ok(match self.fetch(&url, self.options.max_diff_bytes)? {
            Fetch::Found(bytes) => Some(bytes),
            Fetch::NotFound => None,
        })
    }
}
