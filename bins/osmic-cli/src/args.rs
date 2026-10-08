//! Validated argument types.

use std::path::PathBuf;
use std::str::FromStr;

use osmic_core::{BBox, Zoom};
use osmic_osm::{NodeStorage, TagFilter};

/// `min-max` or a single zoom level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZoomRange {
    pub min: u8,
    pub max: u8,
}

impl FromStr for ZoomRange {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        let parse = |v: &str| -> Result<u8, String> {
            let z: u8 = v
                .trim()
                .parse()
                .map_err(|_| format!("'{v}' is not a zoom level"))?;
            if z > Zoom::MAX.get() {
                return Err(format!(
                    "zoom {z} exceeds the maximum of {}",
                    Zoom::MAX.get()
                ));
            }
            Ok(z)
        };
        let (min, max) = match s.split_once('-') {
            Some((a, b)) => (parse(a)?, parse(b)?),
            None => {
                let z = parse(s)?;
                (z, z)
            }
        };
        if min > max {
            return Err(format!("min zoom {min} is greater than max zoom {max}"));
        }
        Ok(Self { min, max })
    }
}

/// `min_lon,min_lat,max_lon,max_lat` in WGS84 degrees.
pub fn parse_bbox(s: &str) -> Result<BBox, String> {
    let parts: Vec<&str> = s.split(',').map(str::trim).collect();
    let [a, b, c, d] = parts[..] else {
        return Err("expected min_lon,min_lat,max_lon,max_lat".into());
    };
    let num = |v: &str| -> Result<f64, String> {
        v.parse::<f64>()
            .ok()
            .filter(|f| f.is_finite())
            .ok_or_else(|| format!("'{v}' is not a number"))
    };
    let bbox = BBox::new(num(a)?, num(b)?, num(c)?, num(d)?);
    if !(-180.0..=180.0).contains(&bbox.min_lon) || !(-180.0..=180.0).contains(&bbox.max_lon) {
        return Err("longitudes must be within -180..180".into());
    }
    if !(-90.0..=90.0).contains(&bbox.min_lat) || !(-90.0..=90.0).contains(&bbox.max_lat) {
        return Err("latitudes must be within -90..90".into());
    }
    if bbox.min_lon > bbox.max_lon || bbox.min_lat > bbox.max_lat {
        return Err("bbox is inverted: expected min_lon,min_lat,max_lon,max_lat".into());
    }
    Ok(bbox)
}

/// `sparse`, `dense[:MAX_ID]` or `file:PATH[:MAX_ID]`.
pub fn parse_node_store(s: &str) -> Result<NodeStorage, String> {
    // 2^34 covers every OSM node id for years to come.
    const DEFAULT_MAX: i64 = 1 << 34;
    let max = |v: Option<&str>| -> Result<i64, String> {
        v.map_or(Ok(DEFAULT_MAX), |m| {
            m.parse::<i64>()
                .ok()
                .filter(|&n| n > 0)
                .ok_or_else(|| format!("'{m}' is not a positive node id"))
        })
    };
    match s.split_once(':') {
        None if s == "sparse" => Ok(NodeStorage::Sparse),
        None if s == "dense" => Ok(NodeStorage::DenseMemory {
            max_node_id: DEFAULT_MAX,
        }),
        Some(("dense", m)) => Ok(NodeStorage::DenseMemory {
            max_node_id: max(Some(m))?,
        }),
        Some(("file", rest)) => {
            let (path, m) = match rest.rsplit_once(':') {
                Some((p, m)) if m.chars().all(|c| c.is_ascii_digit()) && !m.is_empty() => {
                    (p, Some(m))
                }
                _ => (rest, None),
            };
            if path.is_empty() {
                return Err("file: needs a path".into());
            }
            Ok(NodeStorage::DenseFile {
                path: PathBuf::from(path),
                max_node_id: max(m)?,
            })
        }
        _ => Err(format!(
            "unknown node store '{s}' (use sparse, dense[:MAX_ID] or file:PATH[:MAX_ID])"
        )),
    }
}

pub fn parse_filter(s: &str) -> Result<TagFilter, String> {
    TagFilter::parse(s).map_err(|e| e.to_string())
}

/// A finite, non-negative number of meters.
pub fn parse_meters(s: &str) -> Result<f64, String> {
    s.parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v >= 0.0)
        .ok_or_else(|| format!("'{s}' is not a non-negative distance"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zoom_ranges() {
        assert_eq!("0-14".parse(), Ok(ZoomRange { min: 0, max: 14 }));
        assert_eq!("7".parse(), Ok(ZoomRange { min: 7, max: 7 }));
        assert!("14-0".parse::<ZoomRange>().is_err());
        assert!("0-255".parse::<ZoomRange>().is_err());
        assert!("a-b".parse::<ZoomRange>().is_err());
    }

    #[test]
    fn bboxes() {
        assert!(parse_bbox("-122.5,37.7,-122.3,37.8").is_ok());
        assert!(parse_bbox("1,2,3").is_err());
        assert!(parse_bbox("1,2,x,4").is_err());
        assert!(parse_bbox("10,0,0,10").is_err(), "inverted");
        assert!(parse_bbox("0,0,200,10").is_err(), "out of range");
        assert!(parse_bbox("NaN,0,1,1").is_err());
    }

    #[test]
    fn node_stores() {
        assert_eq!(parse_node_store("sparse"), Ok(NodeStorage::Sparse));
        assert!(matches!(
            parse_node_store("dense:100"),
            Ok(NodeStorage::DenseMemory { max_node_id: 100 })
        ));
        assert_eq!(
            parse_node_store("file:/tmp/n.bin:20"),
            Ok(NodeStorage::DenseFile {
                path: "/tmp/n.bin".into(),
                max_node_id: 20
            })
        );
        assert!(matches!(
            parse_node_store("file:C:/nodes.bin"),
            Ok(NodeStorage::DenseFile { .. })
        ));
        assert!(parse_node_store("dense:-1").is_err());
        assert!(parse_node_store("bogus").is_err());
    }

    #[test]
    fn meters() {
        assert_eq!(parse_meters("100"), Ok(100.0));
        assert!(parse_meters("-1").is_err());
        assert!(parse_meters("inf").is_err());
    }
}
