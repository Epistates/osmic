use std::fmt;

use serde::{Deserialize, Serialize};

/// The three OSM element types.
///
/// OSM IDs are only unique *per type*: node 5, way 5 and relation 5 are
/// unrelated objects. Anything that stores or indexes elements of mixed type
/// must key on [`OsmId`], never on the bare `i64`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OsmType {
    Node,
    Way,
    Relation,
}

impl OsmType {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Node => "node",
            Self::Way => "way",
            Self::Relation => "relation",
        }
    }

    /// Single-digit type code used by [`OsmId::vector_tile_id`] and
    /// [`OsmId::to_key`]: node = 1, way = 2, relation = 3.
    pub const fn code(self) -> u8 {
        match self {
            Self::Node => 1,
            Self::Way => 2,
            Self::Relation => 3,
        }
    }

    pub const fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::Node),
            2 => Some(Self::Way),
            3 => Some(Self::Relation),
            _ => None,
        }
    }
}

impl fmt::Display for OsmType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A typed OSM element identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct OsmId {
    pub osm_type: OsmType,
    pub id: i64,
}

impl OsmId {
    pub const fn new(osm_type: OsmType, id: i64) -> Self {
        Self { osm_type, id }
    }

    pub const fn node(id: i64) -> Self {
        Self::new(OsmType::Node, id)
    }

    pub const fn way(id: i64) -> Self {
        Self::new(OsmType::Way, id)
    }

    pub const fn relation(id: i64) -> Self {
        Self::new(OsmType::Relation, id)
    }

    /// Vector-tile feature id: `id * 10 + type_code` (node 1, way 2,
    /// relation 3), the convention used by Planetiler/OpenMapTiles so ids
    /// from different element types never collide.
    ///
    /// Returns `None` for negative ids (locally-created, unuploaded
    /// objects) and ids too large to encode.
    pub fn vector_tile_id(self) -> Option<u64> {
        let id = u64::try_from(self.id).ok()?;
        id.checked_mul(10)?
            .checked_add(u64::from(self.osm_type.code()))
    }

    /// Inverse of [`OsmId::vector_tile_id`].
    pub fn from_vector_tile_id(value: u64) -> Option<Self> {
        let osm_type = OsmType::from_code(u8::try_from(value % 10).ok()?)?;
        let id = i64::try_from(value / 10).ok()?;
        Some(Self::new(osm_type, id))
    }

    /// Order-preserving byte key for embedded databases: the type code
    /// followed by the id in big-endian order with the sign bit flipped,
    /// so keys sort by (type, id) including negative ids.
    pub fn to_key(self) -> [u8; 9] {
        let mut key = [0u8; 9];
        key[0] = self.osm_type.code();
        key[1..].copy_from_slice(&((self.id as u64) ^ (1 << 63)).to_be_bytes());
        key
    }

    /// Inverse of [`OsmId::to_key`].
    pub fn from_key(key: &[u8]) -> Option<Self> {
        let (&code, rest) = key.split_first()?;
        let bytes: [u8; 8] = rest.try_into().ok()?;
        let id = (u64::from_be_bytes(bytes) ^ (1 << 63)) as i64;
        Some(Self::new(OsmType::from_code(code)?, id))
    }
}

impl fmt::Display for OsmId {
    /// Formats as `n123`, `w123` or `r123` (osmium's short notation).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let prefix = match self.osm_type {
            OsmType::Node => 'n',
            OsmType::Way => 'w',
            OsmType::Relation => 'r',
        };
        write!(f, "{prefix}{}", self.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_numeric_id_different_types_are_distinct() {
        assert_ne!(OsmId::node(5), OsmId::way(5));
        assert_ne!(
            OsmId::node(5).vector_tile_id(),
            OsmId::way(5).vector_tile_id()
        );
        assert_ne!(OsmId::node(5).to_key(), OsmId::relation(5).to_key());
    }

    #[test]
    fn vector_tile_id_round_trips() {
        for id in [
            OsmId::node(0),
            OsmId::way(123),
            OsmId::relation(13_000_000_000),
        ] {
            let encoded = id.vector_tile_id().expect("encodable");
            assert_eq!(OsmId::from_vector_tile_id(encoded), Some(id));
        }
        assert_eq!(OsmId::way(42).vector_tile_id(), Some(422));
    }

    #[test]
    fn negative_ids_have_no_vector_tile_id() {
        assert_eq!(OsmId::node(-1).vector_tile_id(), None);
    }

    #[test]
    fn keys_round_trip_and_sort_by_type_then_id() {
        let ids = [
            OsmId::node(-5),
            OsmId::node(0),
            OsmId::node(7),
            OsmId::way(-1),
            OsmId::way(3),
            OsmId::relation(1),
        ];
        for pair in ids.windows(2) {
            assert!(
                pair[0].to_key() < pair[1].to_key(),
                "{} !< {}",
                pair[0],
                pair[1]
            );
        }
        for id in ids {
            assert_eq!(OsmId::from_key(&id.to_key()), Some(id));
        }
    }

    #[test]
    fn display_uses_short_notation() {
        assert_eq!(OsmId::way(42).to_string(), "w42");
    }
}
