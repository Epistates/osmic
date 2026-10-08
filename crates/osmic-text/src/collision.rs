//! Rectangle collision index for label placement.

use rstar::{AABB, RTree, RTreeObject};

/// An axis-aligned rectangle in pixel space (`min` inclusive, `max`
/// exclusive in the overlap test).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub min: [f32; 2],
    pub max: [f32; 2],
}

impl Rect {
    pub const fn new(x0: f32, y0: f32, x1: f32, y1: f32) -> Self {
        Self {
            min: [x0, y0],
            max: [x1, y1],
        }
    }

    pub fn width(&self) -> f32 {
        self.max[0] - self.min[0]
    }

    pub fn height(&self) -> f32 {
        self.max[1] - self.min[1]
    }

    /// The rectangle grown by `amount` on every side.
    pub fn inflate(&self, amount: f32) -> Self {
        Self::new(
            self.min[0] - amount,
            self.min[1] - amount,
            self.max[0] + amount,
            self.max[1] + amount,
        )
    }

    /// Whether the interiors overlap. Rectangles that merely touch do not.
    pub fn overlaps(&self, other: &Rect) -> bool {
        self.min[0] < other.max[0]
            && other.min[0] < self.max[0]
            && self.min[1] < other.max[1]
            && other.min[1] < self.max[1]
    }

    /// The smallest rectangle containing both.
    pub fn union(&self, other: &Rect) -> Rect {
        Rect::new(
            self.min[0].min(other.min[0]),
            self.min[1].min(other.min[1]),
            self.max[0].max(other.max[0]),
            self.max[1].max(other.max[1]),
        )
    }

    fn aabb(&self) -> AABB<[f32; 2]> {
        AABB::from_corners(self.min, self.max)
    }
}

impl RTreeObject for Rect {
    type Envelope = AABB<[f32; 2]>;

    fn envelope(&self) -> Self::Envelope {
        self.aabb()
    }
}

/// Spatial index of occupied rectangles (an R-tree).
///
/// Queries are order-independent, so a placement run is deterministic as
/// long as candidates are offered in a deterministic order.
#[derive(Default)]
pub struct CollisionIndex {
    tree: RTree<Rect>,
}

impl CollisionIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of rectangles stored.
    pub fn len(&self) -> usize {
        self.tree.size()
    }

    pub fn is_empty(&self) -> bool {
        self.tree.size() == 0
    }

    /// Whether `rect` overlaps anything already inserted.
    pub fn collides(&self, rect: &Rect) -> bool {
        self.tree
            .locate_in_envelope_intersecting(&rect.aabb())
            .any(|other| other.overlaps(rect))
    }

    /// Whether any of `rects` collides.
    pub fn collides_any(&self, rects: &[Rect]) -> bool {
        rects.iter().any(|r| self.collides(r))
    }

    /// Occupy `rect`.
    pub fn insert(&mut self, rect: Rect) {
        self.tree.insert(rect);
    }

    /// Insert all of `rects` if none collides; returns whether they were
    /// inserted.
    pub fn try_insert(&mut self, rects: &[Rect]) -> bool {
        if self.collides_any(rects) {
            return false;
        }
        for r in rects {
            self.insert(*r);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlap_is_strict() {
        let a = Rect::new(0.0, 0.0, 10.0, 10.0);
        assert!(a.overlaps(&Rect::new(5.0, 5.0, 15.0, 15.0)));
        assert!(
            !a.overlaps(&Rect::new(10.0, 0.0, 20.0, 10.0)),
            "touching edges"
        );
        assert!(!a.overlaps(&Rect::new(11.0, 0.0, 20.0, 10.0)));
        assert!(a.overlaps(&Rect::new(2.0, 2.0, 3.0, 3.0)), "containment");
    }

    #[test]
    fn index_detects_collisions() {
        let mut index = CollisionIndex::new();
        assert!(index.try_insert(&[Rect::new(0.0, 0.0, 10.0, 10.0)]));
        assert!(!index.try_insert(&[Rect::new(9.0, 9.0, 20.0, 20.0)]));
        assert!(index.try_insert(&[Rect::new(10.0, 0.0, 20.0, 10.0)]));
        assert_eq!(index.len(), 2);
    }

    #[test]
    fn multi_rect_insert_is_all_or_nothing() {
        let mut index = CollisionIndex::new();
        index.insert(Rect::new(100.0, 0.0, 110.0, 10.0));
        let rects = [
            Rect::new(0.0, 0.0, 10.0, 10.0),
            Rect::new(105.0, 0.0, 115.0, 10.0),
        ];
        assert!(!index.try_insert(&rects));
        assert_eq!(index.len(), 1, "no partial insert");
    }

    #[test]
    fn inflate_and_union() {
        let r = Rect::new(0.0, 0.0, 4.0, 2.0).inflate(1.0);
        assert_eq!(r, Rect::new(-1.0, -1.0, 5.0, 3.0));
        assert_eq!(
            r.union(&Rect::new(0.0, 0.0, 10.0, 1.0)),
            Rect::new(-1.0, -1.0, 10.0, 3.0)
        );
        assert_eq!((r.width(), r.height()), (6.0, 4.0));
    }
}
