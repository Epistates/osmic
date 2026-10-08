//! Merging change files into the final state of every touched object.

use std::collections::BTreeMap;

use osmic_core::OsmId;

use crate::osc::{Change, Element};

/// The net effect of one or more change files: for every object touched,
/// its final state (`None` = deleted). Later changes win, so applying a
/// sequence of diffs in order and merging them first are equivalent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChangeSet {
    objects: BTreeMap<OsmId, Option<Element>>,
}

impl ChangeSet {
    /// Add changes in file order.
    pub fn extend(&mut self, changes: impl IntoIterator<Item = Change>) {
        for c in changes {
            self.objects.insert(c.id, c.element);
        }
    }

    /// Objects touched.
    pub fn len(&self) -> usize {
        self.objects.len()
    }

    pub fn is_empty(&self) -> bool {
        self.objects.is_empty()
    }

    /// Final states in (type, id) order — the order of a sorted PBF.
    pub fn iter(&self) -> impl Iterator<Item = (&OsmId, &Option<Element>)> {
        self.objects.iter()
    }

    /// Number of deletions.
    pub fn deletions(&self) -> usize {
        self.objects.values().filter(|e| e.is_none()).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::osc::ChangeAction;
    use osmic_core::FixedCoord;

    fn node(id: i64, lon: i32) -> Change {
        Change {
            action: ChangeAction::Modify,
            id: OsmId::node(id),
            version: None,
            element: Some(Element::Node {
                id,
                location: FixedCoord::new(lon, 0),
                tags: vec![],
            }),
        }
    }

    fn delete(id: OsmId) -> Change {
        Change {
            action: ChangeAction::Delete,
            id,
            version: None,
            element: None,
        }
    }

    #[test]
    fn later_changes_win_and_order_is_type_then_id() {
        let mut cs = ChangeSet::default();
        cs.extend([node(5, 1), delete(OsmId::way(1)), node(2, 1)]);
        cs.extend([node(5, 2), delete(OsmId::node(2))]);
        let ids: Vec<OsmId> = cs.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, [OsmId::node(2), OsmId::node(5), OsmId::way(1)]);
        assert_eq!(cs.deletions(), 2);
        let (_, n5) = cs.iter().nth(1).expect("node 5");
        assert!(matches!(n5, Some(Element::Node { location, .. }) if location.lon == 2));
    }
}
