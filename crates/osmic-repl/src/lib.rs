//! OSM replication: keep a PBF file current with a replication server.
//!
//! [`update_pbf`] reads the replication state stored in the PBF header
//! (written by osmium, pyosmium and Geofabrik, or by a previous run),
//! downloads every newer diff from the server (bounded per run), merges them
//! into one [`ChangeSet`], and applies it in a single streaming pass, writing
//! the new state into the output header. Because the state lives inside the
//! file and the file is replaced atomically, an interrupted run never leaves
//! data and state out of step — re-running simply continues.
//!
//! Regenerate tiles from the updated file with the tile pipeline; for
//! regional extracts, use the region's own replication stream (e.g.
//! Geofabrik's `…-updates/`) so the extract does not accumulate data from
//! elsewhere.

#![warn(missing_docs)]

pub mod apply;
pub mod changeset;
pub mod client;
mod error;
pub mod osc;
pub mod state;

use std::path::Path;

use tracing::info;

pub use apply::{ApplyStats, apply_to_pbf};
pub use changeset::ChangeSet;
pub use client::{ClientOptions, ReplicationClient};
pub use error::ReplError;
pub use osc::{
    Change, ChangeAction, Element, Member, OscLimits, parse_osc, parse_osc_auto_with, parse_osc_gz,
    parse_osc_gz_with, parse_osc_with,
};
pub use state::ReplicationState;

/// Options for [`update_pbf`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct UpdateOptions {
    /// Replication base URL; overrides the one in the PBF header. A server
    /// for a different stream than the header's needs `start_sequence`,
    /// since sequence numbers are per stream.
    pub server: Option<String>,
    /// Sequence the data is current to; overrides the PBF header's.
    pub start_sequence: Option<u64>,
    /// Most diffs applied in one run.
    pub max_diffs: u64,
    /// Stop fetching further diffs in this run once this many objects have
    /// changed (bounds memory; the next run continues).
    pub max_objects: usize,
    /// HTTP client settings.
    pub client: ClientOptions,
    /// Limits applied to each downloaded change file.
    pub osc_limits: OscLimits,
}

impl Default for UpdateOptions {
    fn default() -> Self {
        Self {
            server: None,
            start_sequence: None,
            max_diffs: 1_440,
            max_objects: 20_000_000,
            client: ClientOptions::default(),
            osc_limits: OscLimits::default(),
        }
    }
}

impl UpdateOptions {
    /// Use `server` instead of the URL in the PBF header.
    #[must_use]
    pub fn server(mut self, server: Option<String>) -> Self {
        self.server = server;
        self
    }

    /// Start from `sequence` instead of the PBF header's.
    #[must_use]
    pub fn start_sequence(mut self, sequence: Option<u64>) -> Self {
        self.start_sequence = sequence;
        self
    }

    /// Apply at most `diffs` diffs per run.
    #[must_use]
    pub fn max_diffs(mut self, diffs: u64) -> Self {
        self.max_diffs = diffs;
        self
    }

    /// Stop fetching once `objects` objects have changed.
    #[must_use]
    pub fn max_objects(mut self, objects: usize) -> Self {
        self.max_objects = objects;
        self
    }

    /// HTTP client settings.
    #[must_use]
    pub fn client(mut self, client: ClientOptions) -> Self {
        self.client = client;
        self
    }

    /// Change-file limits.
    #[must_use]
    pub fn osc_limits(mut self, limits: OscLimits) -> Self {
        self.osc_limits = limits;
        self
    }
}

/// What [`update_pbf`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct UpdateReport {
    /// State of the input before the run.
    pub from: ReplicationState,
    /// State written to the output; equal to `from` if nothing was applied.
    pub to: ReplicationState,
    /// Number of diffs applied (`to.sequence - from.sequence`).
    pub diffs_applied: u64,
    /// Newest sequence the server has published.
    pub latest_sequence: u64,
    /// Object and block counts from applying the diffs; all zero if none
    /// were applied.
    pub stats: ApplyStats,
}

impl UpdateReport {
    /// Whether the output is now as current as the server.
    pub fn up_to_date(&self) -> bool {
        self.to.sequence >= self.latest_sequence
    }
}

fn same_stream(a: &str, b: &str) -> bool {
    a.trim_end_matches('/')
        .eq_ignore_ascii_case(b.trim_end_matches('/'))
}

/// Bring `input` up to date and write the result to `output` (which may be
/// the same path). `output` is written even when there is nothing to apply.
///
/// # Errors
///
/// - [`ReplError::State`] if the input has no replication URL or sequence
///   and `options` supplies none, if `options.server` names a different
///   stream than the header without a `start_sequence`, or if the server's
///   state is inconsistent.
/// - [`ReplError::UnsupportedInput`] as for [`apply_to_pbf`].
/// - [`ReplError::InsecureUrl`], [`ReplError::Http`] or
///   [`ReplError::TooLarge`] from the [`ReplicationClient`].
/// - [`ReplError::Osc`] for an invalid diff (the message names its
///   sequence).
/// - [`ReplError::Io`] or [`ReplError::Osm`] reading the input or writing
///   the output. An existing output is only ever replaced by a complete
///   file.
pub fn update_pbf(
    input: &Path,
    output: &Path,
    options: &UpdateOptions,
) -> Result<UpdateReport, ReplError> {
    let header = osmic_osm::pbf::read_header(input)?;
    let base_url = options
        .server
        .clone()
        .or(header.replication_base_url.clone())
        .ok_or_else(|| {
            ReplError::State(format!(
                "{} has no replication URL in its header; pass a server URL",
                input.display()
            ))
        })?;
    if options.start_sequence.is_none()
        && let Some(header_url) = &header.replication_base_url
        && !same_stream(header_url, &base_url)
    {
        return Err(ReplError::State(format!(
            "{} was updated from {header_url}; sequence numbers differ between streams, \
             so pass the starting sequence for {base_url}",
            input.display()
        )));
    }
    let sequence = options
        .start_sequence
        .or(header
            .replication_sequence
            .and_then(|s| u64::try_from(s).ok()))
        .ok_or_else(|| {
            ReplError::State(format!(
                "{} has no replication sequence in its header; pass a start sequence",
                input.display()
            ))
        })?;
    let from = ReplicationState {
        sequence,
        timestamp: header.replication_timestamp.and_then(state::format_iso8601),
        base_url: base_url.clone(),
    };

    let client = ReplicationClient::new(&base_url, options.client.clone())?;
    let latest = client.latest_state()?;
    let target = latest
        .sequence
        .min(sequence.saturating_add(options.max_diffs));
    info!(
        current = sequence,
        latest = latest.sequence,
        target,
        "Replication status"
    );

    let mut changes = ChangeSet::default();
    let mut applied = sequence;
    while applied < target && changes.len() < options.max_objects {
        let next = applied + 1;
        let Some(bytes) = client.diff(next)? else {
            break; // published state.txt but diff not visible yet
        };
        osc::parse_osc_auto_with(&bytes, options.osc_limits, |c| {
            changes.insert(c);
            Ok(())
        })
        .map_err(|e| match e {
            ReplError::Osc(m) => ReplError::Osc(format!("diff {next}: {m}")),
            other => other,
        })?;
        applied = next;
    }
    if applied == sequence {
        if output != input {
            copy_atomically(input, output)?;
        }
        return Ok(UpdateReport {
            to: from.clone(),
            from,
            diffs_applied: 0,
            latest_sequence: latest.sequence,
            stats: ApplyStats::default(),
        });
    }
    let to = match client.state(applied)? {
        Some(s) if s.sequence != applied => {
            return Err(ReplError::State(format!(
                "the server's state file for sequence {applied} says sequence {}",
                s.sequence
            )));
        }
        Some(s) => s,
        // Not published: the latest state's timestamp is right if we reached it.
        None => ReplicationState {
            sequence: applied,
            timestamp: (applied == latest.sequence)
                .then(|| latest.timestamp.clone())
                .flatten(),
            base_url: base_url.clone(),
        },
    };
    info!(
        diffs = applied - sequence,
        objects = changes.len(),
        "Applying changes"
    );
    let stats = apply_to_pbf(input, output, &changes, &to)?;
    Ok(UpdateReport {
        from,
        to,
        diffs_applied: applied - sequence,
        latest_sequence: latest.sequence,
        stats,
    })
}

/// Copy `input` to `output` through a temporary file and an atomic rename.
fn copy_atomically(input: &Path, output: &Path) -> Result<(), ReplError> {
    let mut temp = osmic_core::fs::temp_file_for(output)?;
    std::io::copy(&mut std::fs::File::open(input)?, temp.as_file_mut())?;
    osmic_core::fs::persist(temp, output, true)?;
    Ok(())
}
