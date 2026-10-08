use geo_types::{Coord, LineString, Point, Polygon};

use osmic_core::NodeLocationStore;
use osmic_core::error::OsmicResult;
use osmic_core::geometry::Geometry;
use osmic_osm::LayerSet;
use osmic_osm::classify::classify;
use osmic_osm::feature::Feature;
use osmic_osm::tags::{TagStore, Tags};
use osmic_tiles::TileGeneratorConfig;
use tracing::info;

use crate::dirty::DirtyTileSet;
use crate::osc::{ChangeAction, OscChange, OscElement};
use crate::store::FeatureStore;

/// Apply a set of OSC changes to the feature store.
/// Returns the set of tiles that need regeneration.
pub fn apply_changes(
    changes: &[OscChange],
    store: &FeatureStore,
    node_store: &dyn NodeLocationStore,
    tag_store: &TagStore,
    layers: &LayerSet,
    config: &TileGeneratorConfig,
) -> OsmicResult<DirtyTileSet> {
    let mut dirty = DirtyTileSet::new();
    let mut created = 0u64;
    let mut modified = 0u64;
    let mut deleted = 0u64;
    let mut skipped = 0u64;

    for change in changes {
        match change.action {
            ChangeAction::Create | ChangeAction::Modify => {
                if let Some(feature) = build_feature(&change.element, node_store, tag_store, layers)
                {
                    let new_bbox = feature.bbox();

                    // Upsert returns old bbox if feature existed
                    let old_bbox = store.upsert(feature.id, &new_bbox)?;

                    // Mark both old and new tile regions as dirty
                    if let Some(old) = old_bbox {
                        dirty.mark_bbox(&old, config.min_zoom, config.max_zoom);
                    }
                    dirty.mark_bbox(&new_bbox, config.min_zoom, config.max_zoom);

                    if change.action == ChangeAction::Create {
                        created += 1;
                    } else {
                        modified += 1;
                    }
                } else {
                    skipped += 1;
                }
            }
            ChangeAction::Delete => {
                let id = change.element.id();
                if let Some(old_bbox) = store.delete(id)? {
                    dirty.mark_bbox(&old_bbox, config.min_zoom, config.max_zoom);
                    deleted += 1;
                }
            }
        }
    }

    info!(
        created,
        modified,
        deleted,
        skipped,
        dirty_tiles = dirty.len(),
        "Changes applied"
    );

    Ok(dirty)
}

/// Build an osmic Feature from an OSC element.
/// Returns None if the element doesn't classify into a known feature type.
fn build_feature(
    element: &OscElement,
    node_store: &dyn NodeLocationStore,
    tag_store: &TagStore,
    layers: &LayerSet,
) -> Option<Feature> {
    match element {
        OscElement::Node {
            id, lon, lat, tags, ..
        } => {
            let interned_tags = intern_tags(tags, tag_store);
            let kind = classify(&interned_tags, tag_store, layers)?;
            Some(Feature {
                id: *id,
                kind,
                geometry: Geometry::Point(Point::new(*lon, *lat)),
                tags: interned_tags,
            })
        }
        OscElement::Way {
            id,
            node_refs,
            tags,
            ..
        } => {
            // Resolve node references to coordinates
            let coords: Vec<Coord<f64>> = node_refs
                .iter()
                .filter_map(|&nid| {
                    let lonlat = node_store.get(nid)?;
                    Some(Coord {
                        x: lonlat.lon,
                        y: lonlat.lat,
                    })
                })
                .collect();

            if coords.len() < 2 {
                return None;
            }

            let interned_tags = intern_tags(tags, tag_store);
            let kind = classify(&interned_tags, tag_store, layers)?;

            // Closed ways that are area types become polygons
            let is_closed = coords.len() >= 4 && coords.first() == coords.last();
            let mut geometry = if is_closed && kind.is_area() {
                Geometry::Polygon(Polygon::new(LineString::new(coords), vec![]))
            } else {
                Geometry::Line(LineString::new(coords))
            };
            osmic_geo::orient::orient_geometry(&mut geometry);

            Some(Feature {
                id: *id,
                kind,
                geometry,
                tags: interned_tags,
            })
        }
        OscElement::Relation { .. } => {
            // Multipolygon relations require resolving member ways,
            // which needs a second pass. Skip in incremental updates.
            // TODO: Support relation updates
            None
        }
    }
}

/// Intern tag string pairs into the TagStore's compact representation.
fn intern_tags(tags: &[(String, String)], tag_store: &TagStore) -> Tags {
    let mut interned = Tags::new();
    for (k, v) in tags {
        let key = tag_store.intern_key(k);
        let val = tag_store.intern_value(v);
        interned.push(key, val);
    }
    interned
}
