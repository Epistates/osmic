use crate::feature::*;
use crate::layers::LayerSet;
use crate::tags::{TagStore, Tags, WellKnownKey};

/// Classify an OSM element's tags into a `FeatureKind`.
///
/// Returns `None` for elements that don't match any renderable category
/// or if the matching layer is disabled in `layers`.
/// Priority order matches typical map rendering importance.
pub fn classify(tags: &Tags, store: &TagStore, layers: &LayerSet) -> Option<FeatureKind> {
    // === POI / Business tags checked FIRST ===
    // A building with amenity=restaurant should be classified as a restaurant,
    // not a generic building. Check all POI tags before infrastructure.

    if layers.is_enabled("amenity")
        && let Some(val) = tags.get(store.well_known(WellKnownKey::Amenity))
    {
        return Some(FeatureKind::Amenity(AmenityKind::from_tag_value(
            store.resolve(val),
        )));
    }

    if layers.is_enabled("shop")
        && let Some(val) = tags.get(store.well_known(WellKnownKey::Shop))
    {
        return Some(FeatureKind::Shop(ShopKind::from_tag_value(
            store.resolve(val),
        )));
    }

    if layers.is_enabled("tourism")
        && let Some(val) = tags.get(store.well_known(WellKnownKey::Tourism))
    {
        return Some(FeatureKind::Tourism(TourismKind::from_tag_value(
            store.resolve(val),
        )));
    }

    if layers.is_enabled("office")
        && let Some(val) = tags.get(store.well_known(WellKnownKey::Office))
    {
        return Some(FeatureKind::Office(OfficeKind::from_tag_value(
            store.resolve(val),
        )));
    }

    if layers.is_enabled("healthcare")
        && let Some(val) = tags.get(store.well_known(WellKnownKey::Healthcare))
    {
        return Some(FeatureKind::Healthcare(HealthcareKind::from_tag_value(
            store.resolve(val),
        )));
    }

    if layers.is_enabled("craft")
        && let Some(val) = tags.get(store.well_known(WellKnownKey::Craft))
    {
        return Some(FeatureKind::Craft(CraftKind::from_tag_value(
            store.resolve(val),
        )));
    }

    if layers.is_enabled("historic")
        && let Some(val) = tags.get(store.well_known(WellKnownKey::Historic))
    {
        return Some(FeatureKind::Historic(HistoricKind::from_tag_value(
            store.resolve(val),
        )));
    }

    if layers.is_enabled("club")
        && let Some(val) = tags.get(store.well_known(WellKnownKey::Club))
    {
        return Some(FeatureKind::Club(ClubKind::from_tag_value(
            store.resolve(val),
        )));
    }

    if layers.is_enabled("emergency")
        && let Some(val) = tags.get(store.well_known(WellKnownKey::Emergency))
    {
        return Some(FeatureKind::Emergency(EmergencyKind::from_tag_value(
            store.resolve(val),
        )));
    }

    if layers.is_enabled("education")
        && let Some(val) = tags.get(store.well_known(WellKnownKey::Education))
    {
        return Some(FeatureKind::Education(EducationKind::from_tag_value(
            store.resolve(val),
        )));
    }

    if layers.is_enabled("leisure")
        && let Some(val) = tags.get(store.well_known(WellKnownKey::Leisure))
    {
        return Some(FeatureKind::Leisure(LeisureKind::from_tag_value(
            store.resolve(val),
        )));
    }

    // === Infrastructure / geometry tags ===

    if layers.is_enabled("highway")
        && let Some(val) = tags.get(store.well_known(WellKnownKey::Highway))
    {
        return Some(FeatureKind::Highway(HighwayKind::from_tag_value(
            store.resolve(val),
        )));
    }

    if layers.is_enabled("railway")
        && let Some(val) = tags.get(store.well_known(WellKnownKey::Railway))
    {
        return Some(FeatureKind::Railway(RailwayKind::from_tag_value(
            store.resolve(val),
        )));
    }

    if layers.is_enabled("water") {
        if let Some(val) = tags.get(store.well_known(WellKnownKey::Waterway)) {
            return Some(FeatureKind::Water(WaterKind::from_waterway_value(
                store.resolve(val),
            )));
        }
        if let Some(val) = tags.get(store.well_known(WellKnownKey::Water)) {
            return Some(FeatureKind::Water(WaterKind::from_water_value(
                store.resolve(val),
            )));
        }
    }

    if layers.is_enabled("natural")
        && let Some(val) = tags.get(store.well_known(WellKnownKey::Natural))
    {
        let kind = NaturalKind::from_tag_value(store.resolve(val));
        if kind == NaturalKind::Water {
            return Some(FeatureKind::Water(WaterKind::Lake));
        }
        return Some(FeatureKind::Natural(kind));
    }

    if layers.is_enabled("landuse")
        && let Some(val) = tags.get(store.well_known(WellKnownKey::Landuse))
    {
        return Some(FeatureKind::Landuse(LanduseKind::from_tag_value(
            store.resolve(val),
        )));
    }

    // === Building checked LAST among physical features ===
    // So that building=yes + amenity=cafe → Amenity(Cafe), not Building(Yes)
    if layers.is_enabled("building")
        && let Some(val) = tags.get(store.well_known(WellKnownKey::Building))
    {
        return Some(FeatureKind::Building(BuildingKind::from_tag_value(
            store.resolve(val),
        )));
    }

    // === Metadata layers ===

    if layers.is_enabled("boundary")
        && let Some(val) = tags.get(store.well_known(WellKnownKey::Boundary))
    {
        return Some(FeatureKind::Boundary(BoundaryKind::from_tag_value(
            store.resolve(val),
        )));
    }

    if layers.is_enabled("place")
        && let Some(val) = tags.get(store.well_known(WellKnownKey::Place))
    {
        return Some(FeatureKind::Place(PlaceKind::from_tag_value(
            store.resolve(val),
        )));
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: build a `Tags` with a single key=value pair.
    fn single_tag(store: &TagStore, key: WellKnownKey, value: &str) -> Tags {
        let mut tags = Tags::new();
        tags.push(store.well_known(key), store.intern_value(value));
        tags
    }

    // --- Highway classification ---

    #[test]
    fn highway_tag_classifies_as_highway() {
        let store = TagStore::new();
        let tags = single_tag(&store, WellKnownKey::Highway, "residential");
        let kind = classify(&tags, &store, &LayerSet::all()).expect("must classify");
        assert!(matches!(
            kind,
            FeatureKind::Highway(HighwayKind::Residential)
        ));
    }

    #[test]
    fn highway_motorway_classifies_correctly() {
        let store = TagStore::new();
        let tags = single_tag(&store, WellKnownKey::Highway, "motorway");
        let kind = classify(&tags, &store, &LayerSet::all()).expect("must classify");
        assert!(matches!(kind, FeatureKind::Highway(HighwayKind::Motorway)));
    }

    // --- Building classification ---

    #[test]
    fn building_tag_classifies_as_building() {
        let store = TagStore::new();
        let tags = single_tag(&store, WellKnownKey::Building, "yes");
        let kind = classify(&tags, &store, &LayerSet::all()).expect("must classify");
        assert!(matches!(kind, FeatureKind::Building(BuildingKind::Yes)));
    }

    #[test]
    fn building_house_classifies_correctly() {
        let store = TagStore::new();
        let tags = single_tag(&store, WellKnownKey::Building, "house");
        let kind = classify(&tags, &store, &LayerSet::all()).expect("must classify");
        assert!(matches!(kind, FeatureKind::Building(BuildingKind::House)));
    }

    // --- Priority: highway + building → highway wins ---

    #[test]
    fn highway_wins_over_building() {
        let store = TagStore::new();
        let mut tags = Tags::new();
        tags.push(
            store.well_known(WellKnownKey::Highway),
            store.intern_value("primary"),
        );
        tags.push(
            store.well_known(WellKnownKey::Building),
            store.intern_value("yes"),
        );
        let kind = classify(&tags, &store, &LayerSet::all()).expect("must classify");
        assert!(
            matches!(kind, FeatureKind::Highway(_)),
            "highway must take priority over building, got {:?}",
            kind
        );
    }

    // --- natural=water → Water(Lake) ---

    #[test]
    fn natural_water_classifies_as_water_lake() {
        let store = TagStore::new();
        let tags = single_tag(&store, WellKnownKey::Natural, "water");
        let kind = classify(&tags, &store, &LayerSet::all()).expect("must classify");
        assert!(
            matches!(kind, FeatureKind::Water(WaterKind::Lake)),
            "natural=water must produce Water(Lake), got {:?}",
            kind
        );
    }

    // --- No renderable tags → None ---

    #[test]
    fn no_renderable_tags_returns_none() {
        let store = TagStore::new();
        let mut tags = Tags::new();
        // Only non-renderable tags.
        tags.push(
            store.well_known(WellKnownKey::Name),
            store.intern_value("Test Street"),
        );
        tags.push(
            store.well_known(WellKnownKey::Maxspeed),
            store.intern_value("50"),
        );
        let result = classify(&tags, &store, &LayerSet::all());
        assert!(result.is_none(), "non-renderable tags must return None");
    }

    #[test]
    fn empty_tags_returns_none() {
        let store = TagStore::new();
        let tags = Tags::new();
        assert!(classify(&tags, &store, &LayerSet::all()).is_none());
    }

    // --- Waterway vs Water tag priority ---

    #[test]
    fn waterway_tag_classifies_as_water() {
        let store = TagStore::new();
        let tags = single_tag(&store, WellKnownKey::Waterway, "river");
        let kind = classify(&tags, &store, &LayerSet::all()).expect("must classify");
        assert!(
            matches!(kind, FeatureKind::Water(WaterKind::River)),
            "waterway=river must produce Water(River), got {:?}",
            kind
        );
    }

    #[test]
    fn water_tag_classifies_as_lake() {
        let store = TagStore::new();
        let tags = single_tag(&store, WellKnownKey::Water, "lake");
        let kind = classify(&tags, &store, &LayerSet::all()).expect("must classify");
        assert!(
            matches!(kind, FeatureKind::Water(WaterKind::Lake)),
            "water=lake must produce Water(Lake), got {:?}",
            kind
        );
    }

    #[test]
    fn waterway_wins_over_water_tag() {
        // waterway= is checked before water= in the priority chain.
        let store = TagStore::new();
        let mut tags = Tags::new();
        tags.push(
            store.well_known(WellKnownKey::Waterway),
            store.intern_value("river"),
        );
        tags.push(
            store.well_known(WellKnownKey::Water),
            store.intern_value("lake"),
        );
        let kind = classify(&tags, &store, &LayerSet::all()).expect("must classify");
        assert!(
            matches!(kind, FeatureKind::Water(WaterKind::River)),
            "waterway must take priority over water tag, got {:?}",
            kind
        );
    }
}
