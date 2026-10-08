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
pub use osc::{Change, ChangeAction, Element, Member, OscLimits, parse_osc, parse_osc_gz};
pub use state::ReplicationState;

/// Options for [`update_pbf`].
#[derive(Debug, Clone)]
pub struct UpdateOptions {
    /// Replication base URL; overrides the one in the PBF header.
    pub server: Option<String>,
    /// Starting sequence when the PBF header has none.
    pub start_sequence: Option<u64>,
    /// Most diffs applied in one run (bounds memory and run time).
    pub max_diffs: u64,
    pub client: ClientOptions,
    pub osc_limits: OscLimits,
}

impl Default for UpdateOptions {
    fn default() -> Self {
        Self {
            server: None,
            start_sequence: None,
            max_diffs: 1_440,
            client: ClientOptions::default(),
            osc_limits: OscLimits::default(),
        }
    }
}

/// What [`update_pbf`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateReport {
    pub from: ReplicationState,
    pub to: ReplicationState,
    pub diffs_applied: u64,
    /// Newest sequence the server has published.
    pub latest_sequence: u64,
    pub stats: ApplyStats,
}

impl UpdateReport {
    /// Whether the output is now as current as the server.
    pub fn up_to_date(&self) -> bool {
        self.to.sequence >= self.latest_sequence
    }
}

/// Bring `input` up to date and write the result to `output` (which may be
/// the same path).
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
        timestamp: header.replication_timestamp.map(state::format_iso8601),
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
    while applied < target {
        let next = applied + 1;
        let Some(bytes) = client.diff(next)? else {
            break; // published state.txt but diff not visible yet
        };
        changes.extend(parse_osc_gz(&bytes[..], options.osc_limits)?);
        applied = next;
    }
    if applied == sequence {
        return Ok(UpdateReport {
            to: from.clone(),
            from,
            diffs_applied: 0,
            latest_sequence: latest.sequence,
            stats: ApplyStats::default(),
        });
    }
    let to = match client.state(applied)? {
        Some(s) => s,
        None => ReplicationState {
            sequence: applied,
            timestamp: None,
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
