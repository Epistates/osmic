//! The external sort's record format for rendered pieces.
//!
//! A compact varint encoding of a [`TileFeature`], private to the
//! generator: it is written by [`TileGenerator::add`] and read back by
//! [`TileGenerator::finish`] within one run, and is unrelated to MVT.
//!
//! A record is the geometry part ([`encode_geometry`]) followed by the
//! attribute part ([`encode_attributes`]). The attribute part is the same
//! for every piece of a feature, so it is encoded once and appended to each.
//!
//! [`TileGenerator::add`]: crate::TileGenerator::add
//! [`TileGenerator::finish`]: crate::TileGenerator::finish

use crate::model::{GeomType, TileFeature};
use crate::proto::{DecodeError, Reader, put_varint, zigzag_decode32, zigzag_encode32};

/// Append the geometry part of a record: id, geometry type and geometry.
pub(crate) fn encode_geometry(
    id: Option<u64>,
    geom_type: GeomType,
    parts: &[Vec<[i32; 2]>],
    out: &mut Vec<u8>,
) {
    match id {
        Some(id) => {
            out.push(1);
            put_varint(out, id);
        }
        None => out.push(0),
    }
    out.push(geom_type as u8);
    put_varint(out, parts.len() as u64);
    for part in parts {
        put_varint(out, part.len() as u64);
        let (mut px, mut py) = (0i32, 0i32);
        for &[x, y] in part {
            put_varint(out, u64::from(zigzag_encode32(x.wrapping_sub(px))));
            put_varint(out, u64::from(zigzag_encode32(y.wrapping_sub(py))));
            (px, py) = (x, y);
        }
    }
}

/// Append the attribute part of a record.
pub(crate) fn encode_attributes(attributes: &[(String, String)], out: &mut Vec<u8>) {
    put_varint(out, attributes.len() as u64);
    for (k, v) in attributes {
        put_varint(out, k.len() as u64);
        out.extend_from_slice(k.as_bytes());
        put_varint(out, v.len() as u64);
        out.extend_from_slice(v.as_bytes());
    }
}

/// Decode a whole record. Corrupt data is an error, never a panic or an
/// outsized allocation.
pub(crate) fn decode(data: &[u8]) -> Result<TileFeature, DecodeError> {
    let invalid = |detail: &str| DecodeError::Invalid {
        what: "tile feature record",
        detail: detail.to_string(),
    };
    let mut r = Reader::new(data);
    let byte = |r: &mut Reader<'_>| -> Result<u8, DecodeError> { Ok(r.bytes(1)?[0]) };
    let id = match byte(&mut r)? {
        0 => None,
        _ => Some(r.varint()?),
    };
    let geom_type = GeomType::from_u8(byte(&mut r)?).ok_or_else(|| invalid("geometry type"))?;
    let count = |r: &mut Reader<'_>| -> Result<usize, DecodeError> {
        let n = r.varint()?;
        // Every element takes at least one byte, so a count larger than
        // the record is corrupt (and must not drive an allocation).
        usize::try_from(n)
            .ok()
            .filter(|&n| n <= data.len())
            .ok_or_else(|| invalid("count exceeds record length"))
    };
    let n_parts = count(&mut r)?;
    let mut parts = Vec::with_capacity(n_parts);
    for _ in 0..n_parts {
        let n = count(&mut r)?;
        let mut part = Vec::with_capacity(n);
        let (mut x, mut y) = (0i32, 0i32);
        for _ in 0..n {
            let dx = u32::try_from(r.varint()?).map_err(|_| invalid("delta"))?;
            let dy = u32::try_from(r.varint()?).map_err(|_| invalid("delta"))?;
            x = x.wrapping_add(zigzag_decode32(dx));
            y = y.wrapping_add(zigzag_decode32(dy));
            part.push([x, y]);
        }
        parts.push(part);
    }
    let n_attrs = count(&mut r)?;
    let mut attributes = Vec::with_capacity(n_attrs);
    let string = |r: &mut Reader<'_>| -> Result<String, DecodeError> {
        let n = count(r)?;
        let bytes = r.bytes(n)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| invalid("utf-8"))
    };
    for _ in 0..n_attrs {
        let k = string(&mut r)?;
        let v = string(&mut r)?;
        attributes.push((k, v));
    }
    Ok(TileFeature {
        id,
        geom_type,
        parts,
        attributes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(f: &TileFeature) -> Vec<u8> {
        let mut buf = Vec::new();
        encode_geometry(f.id, f.geom_type, &f.parts, &mut buf);
        encode_attributes(&f.attributes, &mut buf);
        buf
    }

    fn sample() -> TileFeature {
        TileFeature {
            id: Some(422),
            geom_type: GeomType::Polygon,
            parts: vec![
                vec![[0, 0], [100, 0], [100, 100], [0, 100]],
                vec![[10, 10], [10, 20], [20, 20], [20, 10]],
                vec![[-64, 4100], [4160, -64], [i32::MAX, i32::MIN]],
            ],
            attributes: vec![
                ("class".into(), "grass".into()),
                ("name".into(), "Ünïcode ✓".into()),
            ],
        }
    }

    #[test]
    fn record_round_trip() {
        let f = sample();
        assert_eq!(decode(&encode(&f)), Ok(f));
        let point = TileFeature {
            id: None,
            geom_type: GeomType::Point,
            parts: vec![vec![[1, 2]]],
            attributes: vec![],
        };
        assert_eq!(decode(&encode(&point)), Ok(point));
    }

    #[test]
    fn truncated_records_error_instead_of_panicking() {
        let buf = encode(&sample());
        for len in 0..buf.len() {
            assert!(decode(&buf[..len]).is_err(), "prefix {len}");
        }
    }
}
