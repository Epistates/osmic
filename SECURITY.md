# Security policy

## Reporting a vulnerability

Please report vulnerabilities privately through GitHub's
[private vulnerability reporting](https://github.com/Epistates/osmic/security/advisories/new)
rather than in a public issue. Include the affected crate and version, a
description of the issue and, if possible, a reproducing input (for example
a small `.osm.pbf`, `.osc.gz` or `.pmtiles` file).

You can expect an acknowledgement within a few working days. Fixes are
released as patch versions of the affected crates and announced in the
changelog and a GitHub security advisory.

## Supported versions

osmic is pre-1.0. Security fixes are made on the latest released minor
version only.

## Scope

osmic parses untrusted input: OSM PBF and XML change files, GeoJSON, MVT
tiles, PMTiles archives and HTTP requests to the tile server. Crashes,
unbounded memory use, path traversal or incorrect output caused by such
input are in scope. Denial of service through legitimately huge inputs (for
example processing a planet file on a small machine) is not.

Dependencies are checked in CI with `cargo deny`; see `deny.toml` for the
advisories that are acknowledged and why.
