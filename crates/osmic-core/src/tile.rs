//! Zoom levels and slippy-map tile coordinates.

use serde::{Deserialize, Serialize};
use std::fmt;

use crate::bbox::BBox;

/// Map zoom level, always within [`Zoom::MIN`]`..=`[`Zoom::MAX`] (0–22).
///
/// Construct with [`Zoom::new`] (checked), [`Zoom::clamped`] or
/// `Zoom::try_from(u8)`; deserialisation rejects levels above the maximum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
pub struct Zoom(u8);

impl Zoom {
    /// The lowest zoom level, 0: one tile for the whole world.
    pub const MIN: Zoom = Zoom(0);
    /// The highest supported zoom level, 22.
    pub const MAX: Zoom = Zoom(22);

    /// Zoom level `z`, or `None` if it exceeds [`Zoom::MAX`].
    pub const fn new(z: u8) -> Option<Self> {
        if z <= Self::MAX.0 {
            Some(Self(z))
        } else {
            None
        }
    }

    /// Zoom level `z`, lowered to [`Zoom::MAX`] if it exceeds it.
    pub const fn clamped(z: u8) -> Self {
        if z <= Self::MAX.0 { Self(z) } else { Self::MAX }
    }

    /// The level as a number.
    pub const fn get(self) -> u8 {
        self.0
    }

    /// Number of tiles along one axis at this zoom.
    pub const fn num_tiles(self) -> u64 {
        1u64 << self.0
    }

    /// Total number of tiles at this zoom (num_tiles^2).
    pub const fn total_tiles(self) -> u64 {
        let n = self.num_tiles();
        n * n
    }
}

impl fmt::Display for Zoom {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "z{}", self.0)
    }
}

/// A zoom level above [`Zoom::MAX`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZoomOutOfRange {
    /// The rejected level.
    pub zoom: u8,
}

impl fmt::Display for ZoomOutOfRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "zoom {} exceeds the maximum of {}",
            self.zoom,
            Zoom::MAX.0
        )
    }
}

impl std::error::Error for ZoomOutOfRange {}

impl TryFrom<u8> for Zoom {
    type Error = ZoomOutOfRange;

    fn try_from(zoom: u8) -> Result<Self, Self::Error> {
        Self::new(zoom).ok_or(ZoomOutOfRange { zoom })
    }
}

impl From<Zoom> for u8 {
    fn from(z: Zoom) -> Self {
        z.0
    }
}

/// Slippy map tile coordinate (x, y, z).
///
/// XYZ scheme: `(0, 0)` is the north-west tile and y grows southward (not
/// TMS). Displays as `z/x/y`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TileCoord {
    /// Column, `0..2^z`, from west to east.
    pub x: u32,
    /// Row, `0..2^z`, from north to south.
    pub y: u32,
    /// Zoom level.
    pub z: Zoom,
}

impl TileCoord {
    /// Tile `(x, y)` at zoom `z`. Unlike [`TileCoord::try_new`], `x` and `y`
    /// are not checked against the grid at `z`.
    pub fn new(x: u32, y: u32, z: Zoom) -> Self {
        Self { x, y, z }
    }

    /// Returns `None` if `z` exceeds [`Zoom::MAX`] or `x`/`y` are outside
    /// the tile grid at that zoom.
    pub fn try_new(x: u32, y: u32, z: u8) -> Option<Self> {
        if z > Zoom::MAX.0 {
            return None;
        }
        let n = crate::mercator::tiles_per_axis(z);
        (x < n && y < n).then_some(Self { x, y, z: Zoom(z) })
    }

    /// Geographic bounding box of this tile.
    pub fn bbox(&self) -> BBox {
        crate::mercator::tile_bounds(self.x, self.y, self.z.0)
    }

    /// Parent tile (one zoom level up).
    pub fn parent(&self) -> Option<Self> {
        if self.z.0 == 0 {
            return None;
        }
        Some(Self {
            x: self.x / 2,
            y: self.y / 2,
            z: Zoom(self.z.0 - 1),
        })
    }

    /// Four child tiles (one zoom level down), or `None` at [`Zoom::MAX`].
    pub fn children(&self) -> Option<[Self; 4]> {
        if self.z.0 >= Zoom::MAX.0 {
            return None;
        }
        let cz = Zoom(self.z.0 + 1);
        let cx = self.x * 2;
        let cy = self.y * 2;
        Some([
            Self::new(cx, cy, cz),
            Self::new(cx + 1, cy, cz),
            Self::new(cx, cy + 1, cz),
            Self::new(cx + 1, cy + 1, cz),
        ])
    }
}

impl fmt::Display for TileCoord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}/{}", self.z.0, self.x, self.y)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn z(level: u8) -> Zoom {
        Zoom::new(level).expect("valid zoom")
    }

    // --- Zoom construction ---

    #[test]
    fn zoom_is_validated_everywhere() {
        assert_eq!(Zoom::new(22).map(Zoom::get), Some(22));
        assert_eq!(Zoom::new(23), None);
        assert_eq!(Zoom::new(u8::MAX), None);
        assert_eq!(Zoom::clamped(64), Zoom::MAX);
        assert_eq!(Zoom::clamped(7).get(), 7);
        assert_eq!(Zoom::try_from(40), Err(ZoomOutOfRange { zoom: 40 }));
        assert_eq!(u8::from(z(9)), 9);
        // Every constructible zoom has a total tile count.
        assert_eq!(Zoom::clamped(64).num_tiles(), 1 << 22);
        assert_eq!(Zoom::MAX.total_tiles(), 1 << 44);
    }

    #[test]
    fn zoom_serde_rejects_out_of_range_levels() {
        let ok: Zoom = serde_json::from_str("14").expect("valid");
        assert_eq!(ok.get(), 14);
        assert!(serde_json::from_str::<Zoom>("23").is_err());
        assert_eq!(serde_json::to_string(&ok).expect("serialise"), "14");
        let tile: TileCoord = serde_json::from_str(r#"{"x":1,"y":2,"z":3}"#).expect("valid");
        assert_eq!(tile, TileCoord::new(1, 2, z(3)));
        assert!(serde_json::from_str::<TileCoord>(r#"{"x":1,"y":2,"z":64}"#).is_err());
    }

    // --- Zoom::num_tiles ---

    #[test]
    fn num_tiles_at_z0_is_one() {
        assert_eq!(Zoom(0).num_tiles(), 1);
    }

    #[test]
    fn num_tiles_at_z1_is_two() {
        assert_eq!(Zoom(1).num_tiles(), 2);
    }

    #[test]
    fn num_tiles_at_z22_is_correct() {
        assert_eq!(Zoom(22).num_tiles(), 1u64 << 22);
    }

    // --- TileCoord::parent ---

    #[test]
    fn parent_at_z0_is_none() {
        let tile = TileCoord::new(0, 0, Zoom(0));
        assert!(tile.parent().is_none());
    }

    #[test]
    fn parent_at_z1_returns_z0() {
        let tile = TileCoord::new(1, 1, Zoom(1));
        let p = tile.parent().expect("z1 tile must have a parent");
        assert_eq!(p.z, Zoom(0));
        assert_eq!(p.x, 0);
        assert_eq!(p.y, 0);
    }

    // --- children → parent round-trip ---

    #[test]
    fn children_then_parent_roundtrip() {
        let tile = TileCoord::new(3, 5, Zoom(4));
        let kids = tile.children().expect("below max zoom");
        for child in &kids {
            let back = child.parent().expect("child must have parent");
            assert_eq!(
                back, tile,
                "child {:?} did not round-trip to parent {:?}",
                child, tile
            );
        }
    }

    #[test]
    fn children_count_is_four_and_zoom_increments() {
        let tile = TileCoord::new(0, 0, Zoom(0));
        let kids = tile.children().expect("below max zoom");
        assert_eq!(kids.len(), 4);
        for child in &kids {
            assert_eq!(child.z, Zoom(1));
        }
        // The four children of (0,0,z0) must cover (0,0), (1,0), (0,1), (1,1) at z1.
        let mut xs: Vec<u32> = kids.iter().map(|c| c.x).collect();
        let mut ys: Vec<u32> = kids.iter().map(|c| c.y).collect();
        xs.sort();
        ys.sort();
        assert_eq!(xs, vec![0, 0, 1, 1]);
        assert_eq!(ys, vec![0, 0, 1, 1]);
    }

    #[test]
    fn children_at_max_zoom_is_none() {
        assert!(TileCoord::new(0, 0, Zoom::MAX).children().is_none());
    }

    #[test]
    fn try_new_validates_grid() {
        assert!(TileCoord::try_new(1, 1, 1).is_some());
        assert!(TileCoord::try_new(2, 0, 1).is_none());
        assert!(TileCoord::try_new(0, 0, Zoom::MAX.0 + 1).is_none());
    }
}
