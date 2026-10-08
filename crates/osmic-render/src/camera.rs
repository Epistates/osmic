//! A Web Mercator camera.
//!
//! Zoom follows the MapLibre convention: the world is `512 * 2^zoom`
//! pixels wide, so zoom `z` shows vector tiles of zoom `z` at 512 logical
//! pixels each. All projection goes through [`osmic_core::mercator`].
//!
//! The camera is pure state plus math — no windowing or GPU types — so it
//! is shared by the interactive viewer and by static rendering.

use osmic_core::bbox::BBox;
use osmic_core::mercator::{
    MAX_LATITUDE, lat_to_unit_y, lon_to_unit_x, tiles_per_axis, unit_x_to_lon, unit_y_to_lat,
};
use osmic_core::{TileCoord, Zoom};

/// Logical pixels per tile at the tile's own zoom.
pub const TILE_SIZE: f64 = 512.0;

/// Lowest zoom the camera allows.
pub const MIN_ZOOM: f64 = 0.0;

/// Highest zoom the camera allows.
pub const MAX_ZOOM: f64 = 22.0;

/// Highest tile zoom [`Camera::visible_tiles`] accepts (tile indices must
/// fit in `u32`).
pub const MAX_TILE_ZOOM: u8 = 30;

/// Most tiles [`Camera::visible_tiles`] returns; a view that would need
/// more gets none. A 4K screen at 2x needs about 70 tiles at the matching
/// zoom, so the limit only stops requests far above the camera's zoom.
pub const MAX_VISIBLE_TILES: usize = 4096;

/// An affine map from the Mercator unit square to pixels:
/// `px = (unit - origin) * scale`.
///
/// Scene construction projects every coordinate through one of these, for a
/// whole view ([`Camera::pixel_mapping`]) or a single tile
/// ([`PixelMapping::for_tile`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PixelMapping {
    /// Unit-square coordinates of the pixel origin.
    pub origin: [f64; 2],
    /// Pixels per unit.
    pub scale: f64,
}

impl PixelMapping {
    /// Pixel coordinates are tile-local: `(0, 0)` is the tile's top-left
    /// and `TILE_SIZE` its far edge.
    pub fn for_tile(tile: TileCoord) -> Self {
        let n = f64::from(tiles_per_axis(tile.z.get()));
        Self {
            origin: [f64::from(tile.x) / n, f64::from(tile.y) / n],
            scale: TILE_SIZE * n,
        }
    }

    /// Project a WGS84 coordinate.
    #[inline]
    pub fn project(&self, lon: f64, lat: f64) -> [f32; 2] {
        [
            ((lon_to_unit_x(lon) - self.origin[0]) * self.scale) as f32,
            ((lat_to_unit_y(lat) - self.origin[1]) * self.scale) as f32,
        ]
    }
}

/// Where a tile lands on screen: `screen = offset + tile_px * scale`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TileTransform {
    /// Screen position (logical px) of the tile's top-left corner.
    pub offset: [f64; 2],
    /// Screen pixels per tile pixel (`2^(camera zoom - tile zoom)`).
    pub scale: f64,
}

/// A tile in view, with the world copy it appears in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VisibleTile {
    /// The tile, with `x` wrapped into `0..2^z`.
    pub coord: TileCoord,
    /// Which repeat of the world it belongs to (`0` is the central one).
    pub world: i32,
}

/// Map camera: a center, a zoom and the viewport size in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Camera {
    center: [f64; 2],
    zoom: f64,
    width: f64,
    height: f64,
}

impl Camera {
    /// A camera centred on `lon`/`lat`. Latitude is clamped to the
    /// Mercator range, zoom to `MIN_ZOOM..=MAX_ZOOM`, and the viewport to at
    /// least one pixel.
    pub fn new(lon: f64, lat: f64, zoom: f64, width: f64, height: f64) -> Self {
        let mut cam = Self {
            center: [0.5, 0.5],
            zoom: 0.0,
            width: width.max(1.0),
            height: height.max(1.0),
        };
        cam.set_zoom(zoom);
        cam.set_center(lon, lat);
        cam
    }

    /// A camera whose view contains `bbox` with `padding` logical pixels to
    /// spare, at the largest fitting zoom.
    pub fn fit_bbox(bbox: &BBox, width: f64, height: f64, padding: f64) -> Self {
        let (x0, x1) = (lon_to_unit_x(bbox.min_lon), lon_to_unit_x(bbox.max_lon));
        let (y0, y1) = (lat_to_unit_y(bbox.max_lat), lat_to_unit_y(bbox.min_lat));
        let (w, h) = (
            (width - 2.0 * padding).max(1.0),
            (height - 2.0 * padding).max(1.0),
        );
        let fit = |extent_px: f64, extent_unit: f64| {
            if extent_unit > 0.0 {
                (extent_px / (TILE_SIZE * extent_unit)).log2()
            } else {
                MAX_ZOOM
            }
        };
        let zoom = fit(w, x1 - x0).min(fit(h, y1 - y0));
        let c = bbox.center();
        Self::new(c.lon, c.lat, zoom, width, height)
    }

    /// The zoom (MapLibre convention).
    pub fn zoom(&self) -> f64 {
        self.zoom
    }

    /// Viewport `[width, height]` in logical pixels.
    pub fn size(&self) -> [f64; 2] {
        [self.width, self.height]
    }

    /// Center as `(lon, lat)`.
    pub fn center(&self) -> (f64, f64) {
        (unit_x_to_lon(self.center[0]), unit_y_to_lat(self.center[1]))
    }

    /// Center in Mercator unit coordinates.
    pub fn center_unit(&self) -> [f64; 2] {
        self.center
    }

    /// World width in logical pixels at the current zoom.
    pub fn world_size(&self) -> f64 {
        TILE_SIZE * self.zoom.exp2()
    }

    /// Set the zoom, clamped to `MIN_ZOOM..=MAX_ZOOM` (NaN is `MIN_ZOOM`).
    pub fn set_zoom(&mut self, zoom: f64) {
        self.zoom = if zoom.is_nan() {
            MIN_ZOOM
        } else {
            zoom.clamp(MIN_ZOOM, MAX_ZOOM)
        };
    }

    /// Re-center; longitude wraps, latitude clamps to the Mercator range.
    pub fn set_center(&mut self, lon: f64, lat: f64) {
        self.set_center_unit([
            lon_to_unit_x(lon),
            lat_to_unit_y(lat.clamp(-MAX_LATITUDE, MAX_LATITUDE)),
        ]);
    }

    /// Re-center in unit coordinates (x wraps, y clamps to `0..=1`).
    pub fn set_center_unit(&mut self, unit: [f64; 2]) {
        let x = if unit[0].is_finite() {
            unit[0].rem_euclid(1.0)
        } else {
            0.5
        };
        let y = if unit[1].is_finite() {
            unit[1].clamp(0.0, 1.0)
        } else {
            0.5
        };
        self.center = [x, y];
    }

    /// Resize the viewport (logical pixels).
    pub fn resize(&mut self, width: f64, height: f64) {
        self.width = width.max(1.0);
        self.height = height.max(1.0);
    }

    /// Unit coordinates of a screen position (not wrapped).
    pub fn screen_to_unit(&self, x: f64, y: f64) -> [f64; 2] {
        let world = self.world_size();
        [
            self.center[0] + (x - self.width / 2.0) / world,
            self.center[1] + (y - self.height / 2.0) / world,
        ]
    }

    /// Geographic coordinates under a screen position.
    pub fn screen_to_lonlat(&self, x: f64, y: f64) -> (f64, f64) {
        let u = self.screen_to_unit(x, y);
        (unit_x_to_lon(u[0]), unit_y_to_lat(u[1].clamp(0.0, 1.0)))
    }

    /// Screen position of a geographic coordinate, in the world copy
    /// nearest the center.
    pub fn lonlat_to_screen(&self, lon: f64, lat: f64) -> [f64; 2] {
        let world = self.world_size();
        let mut dx = lon_to_unit_x(lon) - self.center[0];
        dx -= dx.round(); // nearest copy
        let dy = lat_to_unit_y(lat) - self.center[1];
        [
            self.width / 2.0 + dx * world,
            self.height / 2.0 + dy * world,
        ]
    }

    /// Drag the map by a pointer movement of `(dx, dy)` logical pixels.
    pub fn pan_pixels(&mut self, dx: f64, dy: f64) {
        let world = self.world_size();
        self.set_center_unit([self.center[0] - dx / world, self.center[1] - dy / world]);
    }

    /// Change the zoom by `delta` while keeping the geographic point under
    /// `cursor` (logical pixels) where it is.
    pub fn zoom_at(&mut self, cursor: [f64; 2], delta: f64) {
        let anchor = self.screen_to_unit(cursor[0], cursor[1]);
        self.set_zoom(self.zoom + delta);
        let world = self.world_size();
        self.set_center_unit([
            anchor[0] - (cursor[0] - self.width / 2.0) / world,
            anchor[1] - (cursor[1] - self.height / 2.0) / world,
        ]);
    }

    /// The mapping from the unit square to this view's screen pixels.
    pub fn pixel_mapping(&self) -> PixelMapping {
        let world = self.world_size();
        PixelMapping {
            origin: [
                self.center[0] - self.width / 2.0 / world,
                self.center[1] - self.height / 2.0 / world,
            ],
            scale: world,
        }
    }

    /// The tile zoom to load for this view: the camera zoom rounded down,
    /// capped at the archive's `max_zoom`.
    pub fn tile_zoom(&self, max_zoom: u8) -> u8 {
        (self.zoom.floor() as u8).min(max_zoom)
    }

    /// Tiles at `tile_zoom` intersecting the view grown by `margin_px`,
    /// nearest the center first.
    ///
    /// Returns an empty list for requests that cannot be served: a
    /// `tile_zoom` above [`MAX_TILE_ZOOM`], a non-finite viewport or margin,
    /// or a view needing more than [`MAX_VISIBLE_TILES`] tiles (a tile zoom
    /// far above the camera zoom).
    pub fn visible_tiles(&self, tile_zoom: u8, margin_px: f64) -> Vec<VisibleTile> {
        if tile_zoom > MAX_TILE_ZOOM {
            return Vec::new();
        }
        let n = i64::from(tiles_per_axis(tile_zoom));
        let world = self.world_size();
        let (hw, hh) = (
            (self.width / 2.0 + margin_px) / world,
            (self.height / 2.0 + margin_px) / world,
        );
        if !(hw.is_finite() && hh.is_finite()) {
            return Vec::new();
        }
        // Tile indices as floats first: the counts are checked before any
        // conversion or enumeration.
        let tile_range = |lo: f64, hi: f64| -> (f64, f64) {
            let first = (lo * n as f64).floor();
            (first, ((hi * n as f64).ceil() - 1.0).max(first))
        };
        let (x0, x1) = tile_range(self.center[0] - hw, self.center[0] + hw);
        let (y0, y1) = tile_range(
            (self.center[1] - hh).max(0.0),
            (self.center[1] + hh).min(1.0),
        );
        let (y0, y1) = (y0.clamp(0.0, (n - 1) as f64), y1.clamp(0.0, (n - 1) as f64));
        if (x1 - x0 + 1.0) * (y1 - y0 + 1.0) > MAX_VISIBLE_TILES as f64 {
            return Vec::new();
        }
        let (x0, x1, y0, y1) = (x0 as i64, x1 as i64, y0 as i64, y1 as i64);

        let mut tiles = Vec::new();
        for y in y0..=y1 {
            for x in x0..=x1 {
                let world_copy = x.div_euclid(n);
                let wrapped = x.rem_euclid(n);
                tiles.push(VisibleTile {
                    coord: TileCoord::new(wrapped as u32, y as u32, Zoom::clamped(tile_zoom)),
                    world: world_copy as i32,
                });
            }
        }
        let c = [self.center[0] * n as f64, self.center[1] * n as f64];
        tiles.sort_by(|a, b| {
            let dist = |t: &VisibleTile| {
                let tx = f64::from(t.coord.x) + f64::from(t.world) * n as f64 + 0.5;
                let ty = f64::from(t.coord.y) + 0.5;
                (tx - c[0]).powi(2) + (ty - c[1]).powi(2)
            };
            dist(a).total_cmp(&dist(b))
        });
        tiles
    }

    /// Where `tile` (in world copy `world`) lands on screen.
    pub fn tile_transform(&self, tile: TileCoord, world: i32) -> TileTransform {
        let n = f64::from(tiles_per_axis(tile.z.get()));
        let world_px = self.world_size();
        let unit_x = (f64::from(tile.x) + f64::from(world) * n) / n;
        let unit_y = f64::from(tile.y) / n;
        TileTransform {
            offset: [
                self.width / 2.0 + (unit_x - self.center[0]) * world_px,
                self.height / 2.0 + (unit_y - self.center[1]) * world_px,
            ],
            scale: world_px / (n * TILE_SIZE),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cam() -> Camera {
        Camera::new(-122.4194, 37.7749, 12.3, 1280.0, 800.0)
    }

    #[test]
    fn center_projects_to_the_middle_of_the_screen() {
        let c = cam();
        let (lon, lat) = c.center();
        let p = c.lonlat_to_screen(lon, lat);
        assert!((p[0] - 640.0).abs() < 1e-6 && (p[1] - 400.0).abs() < 1e-6);
    }

    #[test]
    fn mercator_round_trip() {
        let c = cam();
        for (x, y) in [(0.0, 0.0), (1280.0, 800.0), (333.3, 702.1), (640.0, 400.0)] {
            let (lon, lat) = c.screen_to_lonlat(x, y);
            let p = c.lonlat_to_screen(lon, lat);
            assert!(
                (p[0] - x).abs() < 1e-6 && (p[1] - y).abs() < 1e-6,
                "{x},{y} -> {p:?}"
            );
        }
    }

    #[test]
    fn projection_is_mercator_not_equirectangular() {
        // At 60 degrees latitude a degree of latitude spans ~2x as many
        // pixels as at the equator (1 / cos(lat)).
        let c = Camera::new(0.0, 0.0, 6.0, 1000.0, 1000.0);
        let eq = c.lonlat_to_screen(0.0, 1.0)[1] - c.lonlat_to_screen(0.0, 0.0)[1];
        let hi = c.lonlat_to_screen(0.0, 61.0)[1] - c.lonlat_to_screen(0.0, 60.0)[1];
        let ratio = (eq / hi).abs();
        assert!((ratio - 0.5).abs() < 0.02, "{ratio}");
        // Horizontal degrees are the same everywhere.
        let w0 = c.lonlat_to_screen(1.0, 0.0)[0] - c.lonlat_to_screen(0.0, 0.0)[0];
        let w60 = c.lonlat_to_screen(1.0, 60.0)[0] - c.lonlat_to_screen(0.0, 60.0)[0];
        assert!((w0 - w60).abs() < 1e-9);
    }

    #[test]
    fn zoom_at_cursor_keeps_the_point_under_the_cursor() {
        for cursor in [[0.0, 0.0], [1280.0, 800.0], [200.0, 650.0], [640.0, 400.0]] {
            for delta in [1.0, -1.0, 0.37, -2.5] {
                let mut c = cam();
                let before = c.screen_to_lonlat(cursor[0], cursor[1]);
                c.zoom_at(cursor, delta);
                let after = c.screen_to_lonlat(cursor[0], cursor[1]);
                assert!(
                    (before.0 - after.0).abs() < 1e-9 && (before.1 - after.1).abs() < 1e-9,
                    "{cursor:?} {delta}"
                );
                let p = c.lonlat_to_screen(before.0, before.1);
                assert!((p[0] - cursor[0]).abs() < 1e-5 && (p[1] - cursor[1]).abs() < 1e-5);
            }
        }
    }

    #[test]
    fn zoom_at_center_does_not_move_the_center() {
        let mut c = cam();
        let before = c.center();
        c.zoom_at([640.0, 400.0], 2.0);
        let after = c.center();
        assert!((before.0 - after.0).abs() < 1e-9 && (before.1 - after.1).abs() < 1e-9);
        assert!((c.zoom() - 14.3).abs() < 1e-12);
    }

    #[test]
    fn zoom_is_clamped() {
        let mut c = cam();
        c.zoom_at([10.0, 10.0], 100.0);
        assert_eq!(c.zoom(), MAX_ZOOM);
        c.zoom_at([10.0, 10.0], -100.0);
        assert_eq!(c.zoom(), MIN_ZOOM);
    }

    #[test]
    fn latitude_is_clamped_to_the_mercator_range() {
        let c = Camera::new(0.0, 89.9, 3.0, 800.0, 600.0);
        assert!((c.center().1 - MAX_LATITUDE).abs() < 1e-9);
        let mut c = Camera::new(0.0, 0.0, 3.0, 800.0, 600.0);
        c.pan_pixels(0.0, -1.0e9); // drag up: the view moves south
        assert!((c.center().1 + MAX_LATITUDE).abs() < 1e-9);
        c.pan_pixels(0.0, 1.0e12); // drag down: the view moves north
        assert!((c.center().1 - MAX_LATITUDE).abs() < 1e-9);
        c.set_center(0.0, f64::NAN);
        assert!(c.center().1.is_finite());
    }

    #[test]
    fn longitude_wraps() {
        let mut c = Camera::new(179.0, 0.0, 4.0, 800.0, 600.0);
        c.pan_pixels(-2000.0, 0.0);
        assert!(c.center().0 >= -180.0 && c.center().0 <= 180.0);
        let c = Camera::new(190.0, 0.0, 4.0, 800.0, 600.0);
        assert!((c.center().0 - (-170.0)).abs() < 1e-9);
    }

    #[test]
    fn dragging_moves_content_with_the_pointer() {
        let mut c = cam();
        let (lon, lat) = c.center();
        let before = c.lonlat_to_screen(lon, lat);
        c.pan_pixels(100.0, -50.0);
        let after = c.lonlat_to_screen(lon, lat);
        assert!((after[0] - before[0] - 100.0).abs() < 1e-6);
        assert!((after[1] - before[1] + 50.0).abs() < 1e-6);
    }

    #[test]
    fn pixel_mapping_matches_the_camera_projection() {
        let c = cam();
        let m = c.pixel_mapping();
        for (lon, lat) in [(-122.4, 37.8), (-122.5, 37.7)] {
            let a = m.project(lon, lat);
            let b = c.lonlat_to_screen(lon, lat);
            assert!((f64::from(a[0]) - b[0]).abs() < 1e-2 && (f64::from(a[1]) - b[1]).abs() < 1e-2);
        }
    }

    #[test]
    fn tile_mapping_is_tile_local() {
        let tile = TileCoord::new(1309, 3166, Zoom::clamped(13));
        let m = PixelMapping::for_tile(tile);
        let bb = tile.bbox();
        let tl = m.project(bb.min_lon, bb.max_lat);
        let br = m.project(bb.max_lon, bb.min_lat);
        assert!(tl[0].abs() < 1e-2 && tl[1].abs() < 1e-2, "{tl:?}");
        assert!(
            (f64::from(br[0]) - TILE_SIZE).abs() < 1e-2
                && (f64::from(br[1]) - TILE_SIZE).abs() < 1e-2,
            "{br:?}"
        );
    }

    #[test]
    fn tile_transform_agrees_with_projection() {
        let c = cam();
        let tz = c.tile_zoom(14);
        assert_eq!(tz, 12);
        let tiles = c.visible_tiles(tz, 0.0);
        let centre = tiles[0];
        let t = c.tile_transform(centre.coord, centre.world);
        let local = PixelMapping::for_tile(centre.coord);
        let (lon, lat) = c.center();
        let p = local.project(lon, lat);
        let screen = [
            t.offset[0] + f64::from(p[0]) * t.scale,
            t.offset[1] + f64::from(p[1]) * t.scale,
        ];
        let direct = c.lonlat_to_screen(lon, lat);
        assert!(
            (screen[0] - direct[0]).abs() < 0.05 && (screen[1] - direct[1]).abs() < 0.05,
            "{screen:?} {direct:?}"
        );
        assert!((t.scale - 2f64.powf(0.3)).abs() < 1e-9);
    }

    #[test]
    fn visible_tiles_cover_the_view_nearest_first() {
        let c = cam();
        let tiles = c.visible_tiles(12, 0.0);
        let n = 1u32 << 12;
        assert!(
            tiles
                .iter()
                .all(|t| t.coord.x < n && t.coord.y < n && t.coord.z.get() == 12)
        );
        // The first tile contains the center.
        let (lon, lat) = c.center();
        let (cx, cy) = osmic_core::mercator::lonlat_to_tile(lon, lat, 12);
        assert_eq!((tiles[0].coord.x, tiles[0].coord.y), (cx, cy));
        // Every tile intersects the screen; together they cover it.
        let mut covered = 0.0;
        for t in &tiles {
            let tr = c.tile_transform(t.coord, t.world);
            let size = TILE_SIZE * tr.scale;
            let (x0, y0) = (tr.offset[0].max(0.0), tr.offset[1].max(0.0));
            let (x1, y1) = (
                (tr.offset[0] + size).min(1280.0),
                (tr.offset[1] + size).min(800.0),
            );
            assert!(x1 > x0 && y1 > y0, "tile outside view");
            covered += (x1 - x0) * (y1 - y0);
        }
        assert!((covered - 1280.0 * 800.0).abs() < 1.0, "{covered}");
        let with_margin = c.visible_tiles(12, 512.0);
        assert!(with_margin.len() > tiles.len());
    }

    #[test]
    fn world_wraparound_tiles_reference_valid_coordinates() {
        let c = Camera::new(179.9, 0.0, 3.0, 2000.0, 600.0);
        let tiles = c.visible_tiles(3, 0.0);
        assert!(tiles.iter().any(|t| t.world == 1));
        assert!(tiles.iter().all(|t| t.coord.x < 8));
        // Wrapped copies sit to the right of the seam on screen.
        let right = tiles.iter().find(|t| t.world == 1).unwrap();
        let tr = c.tile_transform(right.coord, right.world);
        assert!(tr.offset[0] > 1000.0);
    }

    #[test]
    fn visible_tiles_rejects_unservable_requests_quickly() {
        let start = std::time::Instant::now();
        let c = Camera::new(0.0, 0.0, 2.0, 1024.0, 768.0);
        // z30 tiles for a z2 view would be ~10^17 tiles.
        assert!(c.visible_tiles(30, 0.0).is_empty());
        assert!(c.visible_tiles(MAX_TILE_ZOOM + 1, 0.0).is_empty());
        assert!(c.visible_tiles(u8::MAX, 0.0).is_empty());
        let wide = Camera::new(0.0, 0.0, 2.0, f64::INFINITY, 768.0);
        assert!(wide.visible_tiles(2, 0.0).is_empty());
        assert!(c.visible_tiles(2, f64::INFINITY).is_empty());
        assert!(c.visible_tiles(2, f64::NAN).is_empty());
        assert!(start.elapsed().as_secs() < 1);
        // The matching zoom still works, as does a few zooms above it.
        assert!(!c.visible_tiles(2, 0.0).is_empty());
        let deeper = c.visible_tiles(6, 0.0);
        assert!(!deeper.is_empty() && deeper.len() <= MAX_VISIBLE_TILES);
    }

    #[test]
    fn fit_bbox_contains_the_box() {
        let bb = BBox::new(-122.52, 37.70, -122.35, 37.82);
        let c = Camera::fit_bbox(&bb, 1024.0, 1024.0, 0.0);
        let tl = c.lonlat_to_screen(bb.min_lon, bb.max_lat);
        let br = c.lonlat_to_screen(bb.max_lon, bb.min_lat);
        assert!(
            tl[0] >= -1e-6 && tl[1] >= -1e-6 && br[0] <= 1024.0 + 1e-6 && br[1] <= 1024.0 + 1e-6,
            "{tl:?} {br:?}"
        );
        // It touches at least one pair of edges.
        assert!((br[0] - tl[0] - 1024.0).abs() < 1e-3 || (br[1] - tl[1] - 1024.0).abs() < 1e-3);
    }

    #[test]
    fn tile_zoom_is_floor_capped_at_the_archive() {
        let c = Camera::new(0.0, 0.0, 15.9, 100.0, 100.0);
        assert_eq!(c.tile_zoom(14), 14);
        assert_eq!(c.tile_zoom(20), 15);
        assert_eq!(Camera::new(0.0, 0.0, 0.4, 100.0, 100.0).tile_zoom(14), 0);
    }
}
