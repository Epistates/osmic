//! Tile assembly: sorted records → encoded, compressed tile within a byte
//! budget.
//!
//! Each record's sort key carries its layer, importance and size class (see
//! [`secondary_key`]). If an encoded tile exceeds the budget, the least
//! important (then smallest) features are dropped and the tile re-encoded,
//! so overview tiles stay usable instead of being truncated arbitrarily.
//! Within each layer features are written least important first, so the
//! most important ones draw on top.

use std::io::Write;

use flate2::Compression as GzLevel;
use flate2::write::GzEncoder;

use osmic_core::TileCoord;
use osmic_osm::Layer;

use crate::encode::TileEncoder;
use crate::error::TileError;
use crate::model::{TileFeature, TileLayer};

/// Compression applied to each tile.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum TileCompression {
    /// gzip — what MapLibre, Mapbox and every PMTiles reader expect.
    #[default]
    Gzip,
    /// Uncompressed.
    None,
}

impl TileCompression {
    fn compress(self, data: &[u8]) -> Result<Vec<u8>, TileError> {
        match self {
            Self::None => Ok(data.to_vec()),
            Self::Gzip => {
                let mut gz = GzEncoder::new(Vec::with_capacity(data.len() / 3), GzLevel::default());
                gz.write_all(data)?;
                Ok(gz.finish()?)
            }
        }
    }
}

/// Pack layer, importance and size class into a sort key: layer-major,
/// then most important, then largest first.
pub fn secondary_key(layer: Layer, importance: u8, size_class: u16) -> u64 {
    (u64::from(layer as u8) << 56)
        | (u64::from(255 - importance) << 48)
        | (u64::from(u16::MAX - size_class) << 32)
}

fn layer_of(secondary: u64) -> Option<Layer> {
    Layer::ALL.get((secondary >> 56) as usize).copied()
}

/// Lower sorts first = more important.
fn priority(secondary: u64) -> u64 {
    secondary & 0x00FF_FFFF_0000_0000
}

/// Outcome of assembling one tile.
#[derive(Debug, Clone)]
pub struct AssembledTile {
    /// The tile.
    pub coord: TileCoord,
    /// Encoded and compressed bytes (empty if nothing survived).
    pub data: Vec<u8>,
    /// Features kept in the tile.
    pub features: usize,
    /// Features dropped to meet the byte budget.
    pub dropped: usize,
    /// Encoded size before compression.
    pub raw_bytes: usize,
}

/// Encode one tile from `(secondary_key, feature)` pairs sorted by
/// secondary key.
pub fn assemble(
    coord: TileCoord,
    features: Vec<(u64, TileFeature)>,
    extent: u32,
    encoder: &dyn TileEncoder,
    compression: TileCompression,
    max_bytes: usize,
) -> Result<AssembledTile, TileError> {
    let total = features.len();
    // Rank of each feature in global priority order (lower = kept longer).
    let mut order: Vec<usize> = (0..total).collect();
    order.sort_by_key(|&i| (priority(features[i].0), i));
    let mut rank = vec![0usize; total];
    for (r, &i) in order.iter().enumerate() {
        rank[i] = r;
    }

    // Features arrive sorted by secondary key, so each layer is one run.
    // `ranks[l]` parallels `layers[l].features`.
    let mut layers: Vec<TileLayer> = Vec::new();
    let mut ranks: Vec<Vec<usize>> = Vec::new();
    for (i, (secondary, f)) in features.into_iter().enumerate() {
        let name = layer_of(secondary).map_or("unknown", Layer::as_str);
        if layers.last().is_none_or(|l| l.name != name) {
            layers.push(TileLayer {
                name: name.to_string(),
                extent,
                features: Vec::new(),
            });
            ranks.push(Vec::new());
        }
        if let (Some(l), Some(r)) = (layers.last_mut(), ranks.last_mut()) {
            l.features.push(f);
            r.push(rank[i]);
        }
    }
    for (l, r) in layers.iter_mut().zip(&mut ranks) {
        // Least important first, so the most important draw on top.
        l.features.reverse();
        r.reverse();
    }

    let mut keep = total;
    loop {
        let raw = encoder.encode(&layers)?;
        let data = if raw.is_empty() {
            Vec::new()
        } else {
            compression.compress(&raw)?
        };
        if data.len() <= max_bytes || keep == 0 {
            return Ok(AssembledTile {
                coord,
                data,
                features: keep,
                dropped: total - keep,
                raw_bytes: raw.len(),
            });
        }
        // Scale down proportionally (with headroom) and retry with only the
        // `keep` highest-priority features.
        let ratio = max_bytes as f64 / data.len() as f64;
        let next = ((keep as f64) * ratio * 0.9) as usize;
        keep = next.min(keep - 1);
        for (l, r) in layers.iter_mut().zip(&mut ranks) {
            let features = std::mem::take(&mut l.features);
            let (kept, kept_ranks): (Vec<TileFeature>, Vec<usize>) = features
                .into_iter()
                .zip(r.iter().copied())
                .filter(|&(_, rank)| rank < keep)
                .unzip();
            l.features = kept;
            *r = kept_ranks;
        }
        let mut nonempty = layers.iter().map(|l| !l.features.is_empty());
        ranks.retain(|_| nonempty.next().unwrap_or(false));
        layers.retain(|l| !l.features.is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::MvtEncoder;
    use crate::model::GeomType;
    use osmic_core::Zoom;

    fn point(id: u64, x: i32) -> TileFeature {
        TileFeature {
            id: Some(id),
            geom_type: GeomType::Point,
            parts: vec![vec![[x, x]]],
            attributes: vec![(
                "name".into(),
                format!("feature number {id} with a long name"),
            )],
        }
    }

    #[test]
    fn under_budget_keeps_everything_grouped_by_layer() {
        let feats = vec![
            (secondary_key(Layer::Highway, 100, 10), point(1, 1)),
            (secondary_key(Layer::Highway, 50, 10), point(2, 2)),
            (secondary_key(Layer::Place, 120, 0), point(3, 3)),
        ];
        let t = assemble(
            TileCoord::new(0, 0, Zoom::clamped(0)),
            feats,
            4096,
            &MvtEncoder,
            TileCompression::None,
            1 << 20,
        )
        .expect("assemble");
        assert_eq!((t.features, t.dropped), (3, 0));
        let layers = crate::mvt_decode::decode_layers(&t.data).expect("decode");
        assert_eq!(layers.len(), 2);
        assert_eq!(layers[0].name, "highway");
        // Least important first within the layer.
        let ids: Vec<_> = layers[0].features.iter().map(|f| f.id).collect();
        assert_eq!(ids, [Some(2), Some(1)]);
    }

    #[test]
    fn over_budget_drops_least_important_first() {
        let mut feats: Vec<(u64, TileFeature)> = (0..2_000u64)
            .map(|i| {
                (
                    secondary_key(Layer::Amenity, (i % 200) as u8, 0),
                    point(i, i as i32),
                )
            })
            .collect();
        feats.sort_by_key(|(s, f)| (*s, f.id));
        let t = assemble(
            TileCoord::new(0, 0, Zoom::clamped(0)),
            feats,
            4096,
            &MvtEncoder,
            TileCompression::Gzip,
            8_000,
        )
        .expect("assemble");
        assert!(t.data.len() <= 8_000, "{} bytes", t.data.len());
        assert!(t.dropped > 0 && t.features > 0);
        let gz = flate2::read::GzDecoder::new(&t.data[..]);
        let raw: Vec<u8> = std::io::Read::bytes(gz)
            .collect::<Result<_, _>>()
            .expect("gunzip");
        let layers = crate::mvt_decode::decode_layers(&raw).expect("decode");
        // Everything kept is at least as important as everything dropped:
        // kept ids have the highest (i % 200).
        let min_kept = layers[0]
            .features
            .iter()
            .filter_map(|f| f.id)
            .map(|id| id % 200)
            .min()
            .expect("non-empty");
        assert!(min_kept > 100, "kept a low-importance feature: {min_kept}");
    }
}
