//! Deciding what to draw: which cached tile covers which part of the view.

use osmic_core::mercator::tiles_per_axis;
use osmic_core::{TileCoord, Zoom};
use osmic_render::{Camera, TILE_SIZE, TileTransform, VisibleTile};

/// One tile draw.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DrawItem {
    /// The tile whose geometry is drawn.
    pub source: TileCoord,
    /// Where `source` lands on screen.
    pub transform: TileTransform,
    /// Screen rectangle `[x0, y0, x1, y1]` (logical px) this draw may
    /// cover: the area of the visible tile it stands in for. Each visible
    /// tile is drawn once, scissored to its own area, so a coarser tile
    /// standing in for several missing ones never paints over a finer
    /// neighbour.
    pub region: [f64; 4],
}

/// For every visible tile, pick the best loaded tile: the tile itself, or
/// else its nearest loaded ancestor (a blurrier stand-in while the real tile
/// loads). Tiles with neither are left out and show the background.
pub fn plan_draws(
    camera: &Camera,
    visible: &[VisibleTile],
    is_loaded: impl Fn(&TileCoord) -> bool,
) -> Vec<DrawItem> {
    let mut items = Vec::with_capacity(visible.len());
    for tile in visible {
        let Some((source, world)) = best_source(tile, &is_loaded) else {
            continue;
        };
        let region_t = camera.tile_transform(tile.coord, tile.world);
        let size = TILE_SIZE * region_t.scale;
        items.push(DrawItem {
            source,
            transform: camera.tile_transform(source, world),
            region: [
                region_t.offset[0],
                region_t.offset[1],
                region_t.offset[0] + size,
                region_t.offset[1] + size,
            ],
        });
    }
    items
}

/// The loaded tile covering `tile` and the world copy it belongs to.
fn best_source(
    tile: &VisibleTile,
    is_loaded: &impl Fn(&TileCoord) -> bool,
) -> Option<(TileCoord, i32)> {
    if is_loaded(&tile.coord) {
        return Some((tile.coord, tile.world));
    }
    let z = tile.coord.z.get();
    let n = i64::from(tiles_per_axis(z));
    let unwrapped_x = i64::from(tile.coord.x) + i64::from(tile.world) * n;
    for up in 1..=z {
        let az = z - up;
        let an = i64::from(tiles_per_axis(az));
        let ax = unwrapped_x >> up;
        let ay = tile.coord.y >> up;
        let ancestor = TileCoord::new(ax.rem_euclid(an) as u32, ay, Zoom::clamped(az));
        if is_loaded(&ancestor) {
            return Some((ancestor, ax.div_euclid(an) as i32));
        }
    }
    None
}

/// Convert a logical-pixel rectangle to a physical scissor rectangle
/// `(x, y, w, h)` within a `width x height` target, rounding outward.
/// `None` if nothing is left.
pub fn scissor(region: [f64; 4], scale: f64, width: u32, height: u32) -> Option<[u32; 4]> {
    let x0 = (region[0] * scale).floor().clamp(0.0, f64::from(width));
    let y0 = (region[1] * scale).floor().clamp(0.0, f64::from(height));
    let x1 = (region[2] * scale).ceil().clamp(0.0, f64::from(width));
    let y1 = (region[3] * scale).ceil().clamp(0.0, f64::from(height));
    (x1 > x0 && y1 > y0).then_some([x0 as u32, y0 as u32, (x1 - x0) as u32, (y1 - y0) as u32])
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    fn camera() -> Camera {
        Camera::new(-122.4194, 37.7749, 12.3, 1280.0, 800.0)
    }

    #[test]
    fn loaded_tiles_draw_themselves() {
        let cam = camera();
        let visible = cam.visible_tiles(12, 0.0);
        let loaded: HashSet<TileCoord> = visible.iter().map(|v| v.coord).collect();
        let plan = plan_draws(&cam, &visible, |c| loaded.contains(c));
        assert_eq!(plan.len(), visible.len());
        for (item, v) in plan.iter().zip(&visible) {
            assert_eq!(item.source, v.coord);
            let size = TILE_SIZE * item.transform.scale;
            assert!((item.region[2] - item.region[0] - size).abs() < 1e-9);
            assert_eq!(item.region[0], item.transform.offset[0]);
        }
    }

    #[test]
    fn missing_tiles_fall_back_to_the_nearest_ancestor() {
        let cam = camera();
        let visible = cam.visible_tiles(12, 0.0);
        let first = visible[0].coord;
        let grandparent = first.parent().unwrap().parent().unwrap();
        let parent_of_others: HashSet<TileCoord> = [grandparent].into();
        // Only the grandparent is loaded: every visible tile under it uses it.
        let plan = plan_draws(&cam, &visible, |c| parent_of_others.contains(c));
        assert!(!plan.is_empty());
        let first_item = plan
            .iter()
            .find(|i| (i.region[0] - cam.tile_transform(first, 0).offset[0]).abs() < 1e-9)
            .unwrap();
        assert_eq!(first_item.source, grandparent);
        // The stand-in is drawn at its own (larger) scale but scissored to
        // the missing tile's area.
        assert!(
            (first_item.transform.scale - cam.tile_transform(first, 0).scale * 4.0).abs() < 1e-9
        );
        let region_w = first_item.region[2] - first_item.region[0];
        assert!((region_w - TILE_SIZE * cam.tile_transform(first, 0).scale).abs() < 1e-9);
        // The stand-in covers the missing tile's area.
        let t = first_item.transform;
        let ext = TILE_SIZE * t.scale;
        assert!(
            t.offset[0] <= first_item.region[0] + 1e-6
                && t.offset[0] + ext >= first_item.region[2] - 1e-6
        );
        assert!(
            t.offset[1] <= first_item.region[1] + 1e-6
                && t.offset[1] + ext >= first_item.region[3] - 1e-6
        );
    }

    #[test]
    fn exact_tiles_beat_ancestors() {
        let cam = camera();
        let visible = cam.visible_tiles(12, 0.0);
        let first = visible[0].coord;
        let parent = first.parent().unwrap();
        let loaded: HashSet<TileCoord> = [first, parent].into();
        let plan = plan_draws(&cam, &visible, |c| loaded.contains(c));
        assert!(plan.iter().any(|i| i.source == first));
        assert!(plan.iter().filter(|i| i.source == parent).count() < visible.len());
    }

    #[test]
    fn nothing_loaded_draws_nothing() {
        let cam = camera();
        assert!(plan_draws(&cam, &cam.visible_tiles(12, 0.0), |_| false).is_empty());
    }

    #[test]
    fn wrapped_world_copies_keep_their_world_for_ancestors() {
        let cam = Camera::new(179.9, 0.0, 4.0, 2000.0, 600.0);
        let visible = cam.visible_tiles(4, 0.0);
        let wrapped = visible
            .iter()
            .find(|v| v.world == 1)
            .expect("a tile in the next world copy");
        let root = TileCoord::new(0, 0, Zoom::clamped(0));
        let plan = plan_draws(&cam, std::slice::from_ref(wrapped), |c| *c == root);
        // The root tile is drawn in world copy 1: one world width to the right.
        let expected = cam.tile_transform(root, 1);
        assert_eq!(plan[0].transform, expected);
    }

    #[test]
    fn scissor_rounds_outward_and_clamps() {
        assert_eq!(
            scissor([10.2, 5.7, 20.1, 15.1], 2.0, 100, 100),
            Some([20, 11, 21, 20])
        );
        assert_eq!(
            scissor([-50.0, -50.0, 10.0, 10.0], 1.0, 100, 100),
            Some([0, 0, 10, 10])
        );
        assert_eq!(
            scissor([90.0, 90.0, 500.0, 500.0], 1.0, 100, 100),
            Some([90, 90, 10, 10])
        );
        assert_eq!(scissor([200.0, 0.0, 300.0, 10.0], 1.0, 100, 100), None);
        assert_eq!(scissor([5.0, 5.0, 5.0, 9.0], 1.0, 100, 100), None);
    }
}
