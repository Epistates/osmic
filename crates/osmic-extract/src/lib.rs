//! Business entity extraction from OSM PBF data.
//!
//! Unlike `osmic-osm` which classifies features for map rendering, `osmic-extract`
//! extracts named business entities with contact metadata (phone, website, address)
//! using arbitrary tag-based filtering. Designed for lead generation pipelines.
//!
//! # Pipeline
//!
//! ```text
//! PBF file
//!   → Pass 1: node locations (sparse index) + relations matching the filter
//!   → Pass 2: match nodes and ways, cache member ways of matched relations
//!   → Locate relations (assembled area or member centroid)
//!   → Deduplicate by normalised name + proximity (grid index)
//!   → Write CSV / JSON / GeoJSON atomically
//! ```
//!
//! # GeoJSON output schema
//!
//! [`write_geojson`] emits a `FeatureCollection` where each feature has a
//! `Point` geometry (or `null` when coordinates are unavailable) and the
//! following properties. Only non-empty values are serialized.
//!
//! ## Fixed properties
//!
//! | Property   | Type             | Source                                                    |
//! |------------|------------------|-----------------------------------------------------------|
//! | `name`     | string           | `name` tag                                                |
//! | `osm_type` | string           | `"node"`, `"way"`, or `"relation"`                        |
//! | `osm_id`   | integer          | OSM element ID                                            |
//! | `address`  | string, optional | Joined human-readable form of `addr:*` tags               |
//! | `phone`    | string, optional | `phone`, `contact:phone`, or `telephone` (first non-empty)|
//! | `website`  | string, optional | `website`, `contact:website`, or `url` (first non-empty)  |
//! | `operator` | string, optional | `operator` tag                                            |
//! | `tags`     | string, optional | Remaining tags as `k=v; k=v` (excludes `addr:*`/`contact:*` and the fixed keys above) |
//!
//! ## Structured address pass-through (`addr_*`)
//!
//! Every `addr:*` OSM tag with a non-empty value is surfaced as an
//! additional top-level property. The key transform is:
//!
//! 1. Strip the leading `addr:` namespace.
//! 2. Rewrite any remaining `:` characters to `_`.
//! 3. Re-prefix with `addr_`.
//!
//! So `addr:housenumber` → `addr_housenumber`, `addr:street` → `addr_street`,
//! `addr:street:name` → `addr_street_name`. **Values are passed through
//! verbatim from OSM** — no normalization of state names, country codes,
//! casing, or abbreviations. Consumers that need canonical forms (e.g.
//! `"Arizona"` → `"AZ"`) must apply their own mapping.
//!
//! Commonly seen keys in practice: `addr_housenumber`, `addr_street`,
//! `addr_unit`, `addr_city`, `addr_state`, `addr_postcode`, `addr_country`,
//! `addr_suburb`, `addr_province`, `addr_district`, `addr_place`. The set
//! is not whitelisted — any `addr:*` tag present on the OSM element will
//! appear. Consumers should read the keys they recognize and ignore the rest.
//!
//! ## Example
//!
//! ```json
//! {
//!   "type": "Feature",
//!   "geometry": { "type": "Point", "coordinates": [-112.04, 33.39] },
//!   "properties": {
//!     "name": "Midas",
//!     "osm_type": "node",
//!     "osm_id": 123456789,
//!     "address": "39 East Southern Avenue, Phoenix, AZ, 85040",
//!     "addr_housenumber": "39",
//!     "addr_street": "East Southern Avenue",
//!     "addr_city": "Phoenix",
//!     "addr_state": "AZ",
//!     "addr_postcode": "85040"
//!   }
//! }
//! ```
//!
//! ## JSON vs CSV
//!
//! [`write_json`] emits the same structured `addr_*` fields flat on each
//! entity object (via `#[serde(flatten)]`). [`write_csv`] does **not**
//! emit structured address fields — the CSV column contract is fixed at
//! `name,type,id,lat,lon,address,phone,website,operator,tags`, and the
//! joined `address` column is the only address representation there.

pub mod dedup;
pub mod entity;
pub mod filter;
pub mod output;
pub mod pipeline;

pub use dedup::{deduplicate, normalize_name};
pub use entity::Entity;
pub use filter::{FilterParseError, TagFilter};
pub use output::{OutputError, OutputOptions, write_csv, write_geojson, write_json};
pub use pipeline::{ExtractConfig, ExtractResult, ExtractStats, Extractor};
