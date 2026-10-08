//! Output writers for extracted entities.
//!
//! Every writer streams entities to a temporary file in the destination's
//! directory and atomically renames it into place on success, so a crash
//! or full disk never leaves a truncated file at the destination, and an
//! existing file is only replaced when `overwrite` is set.

use std::io::{self, BufWriter, Write};
use std::path::Path;

use serde::Serialize;

use crate::entity::Entity;

/// Errors from writing output.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum OutputError {
    #[error("{} already exists (refusing to overwrite)", .0.display())]
    Exists(std::path::PathBuf),
    #[error("I/O error writing output")]
    Io(#[from] io::Error),
    #[error("CSV encoding failed")]
    Csv(#[from] csv::Error),
    #[error("JSON encoding failed")]
    Json(#[from] serde_json::Error),
}

/// Output options.
#[derive(Debug, Clone, Copy)]
pub struct OutputOptions {
    /// Replace an existing destination file.
    pub overwrite: bool,
    /// Prefix CSV cells that a spreadsheet would evaluate as formulas
    /// (`=`, `+`, `-`, `@`, tab, carriage return) with `'`. OSM values are
    /// user-supplied, so this is on by default (OWASP "CSV injection").
    pub sanitize_csv_formulas: bool,
}

impl Default for OutputOptions {
    fn default() -> Self {
        Self {
            overwrite: false,
            sanitize_csv_formulas: true,
        }
    }
}

fn write_atomically(
    path: &Path,
    overwrite: bool,
    body: impl FnOnce(&mut BufWriter<&std::fs::File>) -> Result<(), OutputError>,
) -> Result<(), OutputError> {
    if !overwrite && path.exists() {
        return Err(OutputError::Exists(path.to_path_buf()));
    }
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let temp = tempfile::Builder::new()
        .prefix(&osmic_core::fs::temp_file_prefix())
        .suffix(".tmp")
        .tempfile_in(dir)?;
    {
        let mut w = BufWriter::new(temp.as_file());
        body(&mut w)?;
        w.flush()?;
    }
    temp.as_file().sync_all()?;
    if overwrite {
        temp.persist(path).map_err(|e| OutputError::Io(e.error))?;
    } else {
        temp.persist_noclobber(path)
            .map_err(|e| OutputError::Io(e.error))?;
    }
    Ok(())
}

/// Neutralise spreadsheet formula triggers.
fn sanitize(field: &str, enabled: bool) -> std::borrow::Cow<'_, str> {
    if enabled && field.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        std::borrow::Cow::Owned(format!("'{field}"))
    } else {
        std::borrow::Cow::Borrowed(field)
    }
}

/// Write entities as CSV with columns
/// `name,type,id,lat,lon,address,phone,website,operator,tags`.
pub fn write_csv(
    entities: &[Entity],
    path: &Path,
    options: OutputOptions,
) -> Result<(), OutputError> {
    write_atomically(path, options.overwrite, |w| {
        let mut csv = csv::Writer::from_writer(w);
        csv.write_record([
            "name", "type", "id", "lat", "lon", "address", "phone", "website", "operator", "tags",
        ])?;
        let s = options.sanitize_csv_formulas;
        for e in entities {
            let lat = e.lat.map(|v| format!("{v:.7}")).unwrap_or_default();
            let lon = e.lon.map(|v| format!("{v:.7}")).unwrap_or_default();
            csv.write_record([
                sanitize(&e.name, s).as_ref(),
                &e.osm_type,
                &e.osm_id.to_string(),
                &lat,
                &lon,
                sanitize(&e.address, s).as_ref(),
                sanitize(&e.phone, s).as_ref(),
                sanitize(&e.website, s).as_ref(),
                sanitize(&e.operator, s).as_ref(),
                sanitize(&e.tags, s).as_ref(),
            ])?;
        }
        csv.flush()?;
        Ok(())
    })
}

fn write_json_array<T: Serialize>(
    w: &mut impl Write,
    items: impl IntoIterator<Item = T>,
) -> Result<(), OutputError> {
    w.write_all(b"[")?;
    for (i, item) in items.into_iter().enumerate() {
        w.write_all(if i == 0 { b"\n" } else { b",\n" })?;
        serde_json::to_writer(&mut *w, &item)?;
    }
    w.write_all(b"\n]\n")?;
    Ok(())
}

/// Write entities as a JSON array (one entity per line).
pub fn write_json(
    entities: &[Entity],
    path: &Path,
    options: OutputOptions,
) -> Result<(), OutputError> {
    write_atomically(path, options.overwrite, |w| write_json_array(w, entities))
}

/// Write entities as a GeoJSON `FeatureCollection`. Entities without a
/// location get a `null` geometry, as RFC 7946 allows.
pub fn write_geojson(
    entities: &[Entity],
    path: &Path,
    options: OutputOptions,
) -> Result<(), OutputError> {
    write_atomically(path, options.overwrite, |w| {
        w.write_all(br#"{"type":"FeatureCollection","features":"#)?;
        write_json_array(w, entities.iter().map(geojson_feature))?;
        w.write_all(b"}\n")?;
        Ok(())
    })
}

fn geojson_feature(e: &Entity) -> serde_json::Value {
    let geometry = match (e.lon, e.lat) {
        (Some(lon), Some(lat)) => serde_json::json!({"type": "Point", "coordinates": [lon, lat]}),
        _ => serde_json::Value::Null,
    };
    let mut props = serde_json::Map::new();
    props.insert("name".into(), e.name.clone().into());
    props.insert("osm_type".into(), e.osm_type.clone().into());
    props.insert("osm_id".into(), e.osm_id.into());
    for (k, v) in [
        ("address", &e.address),
        ("phone", &e.phone),
        ("website", &e.website),
        ("operator", &e.operator),
        ("tags", &e.tags),
    ] {
        if !v.is_empty() {
            props.insert(k.into(), v.clone().into());
        }
    }
    for (k, v) in &e.address_parts {
        props.insert(k.clone(), v.clone().into());
    }
    serde_json::json!({
        "type": "Feature",
        "id": format!("{}/{}", e.osm_type, e.osm_id),
        "geometry": geometry,
        "properties": props,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use osmic_core::OsmType;

    fn sample() -> Vec<Entity> {
        vec![
            Entity::new(
                OsmType::Node,
                1,
                Some(geo_types::Coord {
                    x: -112.04,
                    y: 33.39,
                }),
                &[
                    ("name", "=HYPERLINK(\"http://evil\")"),
                    ("phone", "+1 555"),
                    ("addr:city", "Phoenix"),
                ],
            ),
            Entity::new(
                OsmType::Relation,
                2,
                None,
                &[("name", "Quote \"me\", please")],
            ),
        ]
    }

    #[test]
    fn csv_is_rfc4180_and_neutralises_formulas() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("out.csv");
        write_csv(&sample(), &path, OutputOptions::default()).expect("write");
        let mut r = csv::Reader::from_path(&path).expect("read");
        let rows: Vec<csv::StringRecord> = r.records().collect::<Result<_, _>>().expect("rows");
        assert_eq!(&rows[0][0], "'=HYPERLINK(\"http://evil\")");
        assert_eq!(
            &rows[0][6], "'+1 555",
            "phone numbers are prefixed too (OWASP)"
        );
        assert_eq!(&rows[1][0], "Quote \"me\", please");
        assert_eq!(&rows[1][3], "", "no location → empty cell");
        let raw = write_csv(
            &sample(),
            &dir.path().join("raw.csv"),
            OutputOptions {
                sanitize_csv_formulas: false,
                ..Default::default()
            },
        );
        assert!(raw.is_ok());
    }

    #[test]
    fn json_and_geojson_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let json = dir.path().join("out.json");
        write_json(&sample(), &json, OutputOptions::default()).expect("json");
        let back: Vec<Entity> =
            serde_json::from_slice(&std::fs::read(&json).expect("read")).expect("parse");
        assert_eq!(back, sample());

        let gj = dir.path().join("out.geojson");
        write_geojson(&sample(), &gj, OutputOptions::default()).expect("geojson");
        let v: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&gj).expect("read")).expect("parse");
        assert_eq!(v["type"], "FeatureCollection");
        assert_eq!(v["features"][0]["properties"]["addr_city"], "Phoenix");
        assert_eq!(v["features"][0]["id"], "node/1");
        assert!(v["features"][1]["geometry"].is_null());
    }

    #[test]
    fn refuses_to_overwrite_unless_asked_and_leaves_no_temp_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("out.json");
        std::fs::write(&path, "keep me").expect("seed");
        assert!(matches!(
            write_json(&sample(), &path, OutputOptions::default()),
            Err(OutputError::Exists(_))
        ));
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "keep me");
        write_json(
            &sample(),
            &path,
            OutputOptions {
                overwrite: true,
                ..Default::default()
            },
        )
        .expect("overwrite");
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .expect("dir")
            .filter_map(Result::ok)
            .map(|e| e.file_name())
            .collect();
        assert_eq!(names.len(), 1, "{names:?}");
    }
}
