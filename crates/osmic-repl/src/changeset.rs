//! Merging change files into the final state of every touched object.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use osmic_core::OsmId;

use crate::osc::{Change, Element};

/// `OsmId` in the order osmium sorts PBF files: by type, then negative ids
/// before positive ones, each by absolute value (`-1, -2, …, 1, 2, …`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SortKey(pub OsmId);

impl SortKey {
    fn parts(self) -> (osmic_core::OsmType, bool, u64) {
        (self.0.osm_type, self.0.id >= 0, self.0.id.unsigned_abs())
    }
}

impl Ord for SortKey {
    fn cmp(&self, other: &Self) -> Ordering {
        self.parts().cmp(&other.parts())
    }
}

impl PartialOrd for SortKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// The net effect of one or more change files: for every object touched,
/// its final state (`None` = deleted).
///
/// When the same object changes several times, the change with the highest
/// version wins; changes without versions (or with equal versions) are
/// resolved by order, later wins. So merging diffs first and applying them
/// one by one are equivalent, even if a diff lists an object out of order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChangeSet {
    objects: BTreeMap<SortKey, (Option<u32>, Option<Element>)>,
}

impl ChangeSet {
    /// Add one change.
    pub fn insert(&mut self, change: Change) {
        let key = SortKey(change.id);
        if let Some((Some(existing), _)) = self.objects.get(&key)
            && change.version.is_some_and(|v| v < *existing)
        {
            return;
        }
        self.objects.insert(key, (change.version, change.element));
    }

    /// Add changes in file order.
    pub fn extend(&mut self, changes: impl IntoIterator<Item = Change>) {
        for c in changes {
            self.insert(c);
        }
    }

    /// Objects touched.
    pub fn len(&self) -> usize {
        self.objects.len()
    }

    /// Whether no object is touched.
    pub fn is_empty(&self) -> bool {
        self.objects.is_empty()
    }

    /// Final states in the order of a sorted PBF (see [`SortKey`]).
    pub fn iter(&self) -> impl Iterator<Item = (&OsmId, &Option<Element>)> {
        self.objects.iter().map(|(k, (_, e))| (&k.0, e))
    }

    /// Whether any object in `first..=last` (PBF order) is touched.
    pub fn touches(&self, first: OsmId, last: OsmId) -> bool {
        self.objects
            .range(SortKey(first)..=SortKey(last))
            .next()
            .is_some()
    }

    /// Number of deletions.
    pub fn deletions(&self) -> usize {
        self.objects.values().filter(|(_, e)| e.is_none()).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::osc::ChangeAction;
    use osmic_core::FixedCoord;

    fn node(id: i64, lon: i32, version: Option<u32>) -> Change {
        Change {
            action: ChangeAction::Modify,
            id: OsmId::node(id),
            version,
            element: Some(Element::Node {
                id,
                location: FixedCoord::new(lon, 0),
                tags: vec![],
                meta: None,
            }),
        }
    }

    fn delete(id: OsmId, version: Option<u32>) -> Change {
        Change {
            action: ChangeAction::Delete,
            id,
            version,
            element: None,
        }
    }

    fn lon_of(cs: &ChangeSet, id: i64) -> Option<i32> {
        cs.iter()
            .find(|(k, _)| **k == OsmId::node(id))
            .and_then(|(_, e)| match e {
                Some(Element::Node { location, .. }) => Some(location.lon),
                _ => None,
            })
    }

    #[test]
    fn later_changes_win_and_order_is_type_then_id() {
        let mut cs = ChangeSet::default();
        cs.extend([
            node(5, 1, None),
            delete(OsmId::way(1), None),
            node(2, 1, None),
        ]);
        cs.extend([node(5, 2, None), delete(OsmId::node(2), None)]);
        let ids: Vec<OsmId> = cs.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, [OsmId::node(2), OsmId::node(5), OsmId::way(1)]);
        assert_eq!(cs.deletions(), 2);
        assert_eq!(lon_of(&cs, 5), Some(2));
    }

    #[test]
    fn the_highest_version_wins() {
        let mut cs = ChangeSet::default();
        cs.extend([node(1, 3, Some(3)), node(1, 2, Some(2))]);
        assert_eq!(
            lon_of(&cs, 1),
            Some(3),
            "an older version listed later loses"
        );
        cs.insert(node(1, 4, Some(4)));
        assert_eq!(lon_of(&cs, 1), Some(4));
        cs.insert(delete(OsmId::node(1), Some(4)));
        assert_eq!(cs.deletions(), 1, "equal versions: later wins");
    }

    #[test]
    fn negative_ids_sort_like_osmium() {
        let mut cs = ChangeSet::default();
        cs.extend([1, -2, 2, -1].map(|id| node(id, 0, None)));
        let ids: Vec<i64> = cs.iter().map(|(id, _)| id.id).collect();
        assert_eq!(ids, [-1, -2, 1, 2]);
        assert!(cs.touches(OsmId::node(-2), OsmId::node(1)));
        assert!(!cs.touches(OsmId::node(3), OsmId::node(9)));
    }
}
