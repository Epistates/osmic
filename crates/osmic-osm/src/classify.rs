//! Tag classification: which layers an OSM element belongs to, and whether a
//! closed way is an area.
//!
//! Classification works directly on borrowed `&str` tags so nothing is
//! interned or allocated for the (vast majority of) elements that do not
//! become features.

use smallvec::SmallVec;

use crate::feature::*;
use crate::layers::{Layer, LayerSet};

/// One layer an element was classified into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Classified<'a> {
    pub kind: FeatureKind,
    /// The tag key that produced the classification (e.g. `"amenity"`).
    pub key: &'static str,
    /// Its raw value (e.g. `"cafe"`), used as the feature's class.
    pub value: &'a str,
}

/// Number of classification-relevant keys tracked by [`KeyValues`].
const KEY_COUNT: usize = 22;

#[derive(Clone, Copy)]
#[repr(usize)]
enum K {
    Amenity,
    Shop,
    Tourism,
    Office,
    Healthcare,
    Craft,
    Historic,
    Club,
    Emergency,
    Education,
    Leisure,
    Highway,
    Railway,
    Waterway,
    Water,
    Natural,
    Landuse,
    Building,
    Boundary,
    Place,
    Area,
    Type,
}

#[inline]
fn key_slot(key: &str) -> Option<K> {
    Some(match key {
        "amenity" => K::Amenity,
        "shop" => K::Shop,
        "tourism" => K::Tourism,
        "office" => K::Office,
        "healthcare" => K::Healthcare,
        "craft" => K::Craft,
        "historic" => K::Historic,
        "club" => K::Club,
        "emergency" => K::Emergency,
        "education" => K::Education,
        "leisure" => K::Leisure,
        "highway" => K::Highway,
        "railway" => K::Railway,
        "waterway" => K::Waterway,
        "water" => K::Water,
        "natural" => K::Natural,
        "landuse" => K::Landuse,
        "building" => K::Building,
        "boundary" => K::Boundary,
        "place" => K::Place,
        "area" => K::Area,
        "type" => K::Type,
        _ => return None,
    })
}

/// The values of every classification-relevant key on one element.
#[derive(Default, Clone, Copy)]
pub struct KeyValues<'a>([Option<&'a str>; KEY_COUNT]);

impl<'a> KeyValues<'a> {
    /// Scan tags once. Empty values are treated as absent.
    pub fn scan(tags: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        let mut v = Self::default();
        for (k, val) in tags {
            if let Some(slot) = key_slot(k)
                && !val.is_empty()
            {
                v.0[slot as usize] = Some(val);
            }
        }
        v
    }

    #[inline]
    fn get(&self, k: K) -> Option<&'a str> {
        self.0[k as usize]
    }

    /// Value of `area=*`, if present.
    pub fn area(&self) -> Option<&'a str> {
        self.get(K::Area)
    }

    /// Value of `type=*` (relations), if present.
    pub fn relation_type(&self) -> Option<&'a str> {
        self.get(K::Type)
    }

    /// True if no classification-relevant key is present.
    pub fn is_empty(&self) -> bool {
        self.0.iter().all(Option::is_none)
    }
}

/// Classify an element into at most one kind per layer, most important
/// first. POI layers (amenity, shop, …) come before the physical layers, so
/// the first entry is the element's primary role; e.g. `amenity=school` +
/// `building=yes` yields `[Amenity(School), Building(Yes)]`.
pub fn classify<'a>(kv: &KeyValues<'a>, layers: LayerSet) -> SmallVec<[Classified<'a>; 2]> {
    let mut out: SmallVec<[Classified<'a>; 2]> = SmallVec::new();
    let mut push = |layer: Layer, key: &'static str, value: &'a str, kind: FeatureKind| {
        if layers.contains(layer) {
            out.push(Classified { kind, key, value });
        }
    };

    macro_rules! simple {
        ($slot:ident, $key:literal, $variant:ident, $kind:ty) => {
            if let Some(v) = kv.get(K::$slot) {
                push(
                    Layer::$variant,
                    $key,
                    v,
                    FeatureKind::$variant(<$kind>::from_tag_value(v)),
                );
            }
        };
    }

    simple!(Amenity, "amenity", Amenity, AmenityKind);
    simple!(Shop, "shop", Shop, ShopKind);
    simple!(Tourism, "tourism", Tourism, TourismKind);
    simple!(Office, "office", Office, OfficeKind);
    simple!(Healthcare, "healthcare", Healthcare, HealthcareKind);
    simple!(Craft, "craft", Craft, CraftKind);
    simple!(Historic, "historic", Historic, HistoricKind);
    simple!(Club, "club", Club, ClubKind);
    simple!(Emergency, "emergency", Emergency, EmergencyKind);
    simple!(Education, "education", Education, EducationKind);
    simple!(Leisure, "leisure", Leisure, LeisureKind);
    simple!(Highway, "highway", Highway, HighwayKind);
    simple!(Railway, "railway", Railway, RailwayKind);

    // Water: waterway=*, then water=*, then natural=water.
    let natural = kv.get(K::Natural);
    if let Some(v) = kv.get(K::Waterway) {
        push(
            Layer::Water,
            "waterway",
            v,
            FeatureKind::Water(WaterKind::from_waterway_value(v)),
        );
    } else if let Some(v) = kv.get(K::Water) {
        push(
            Layer::Water,
            "water",
            v,
            FeatureKind::Water(WaterKind::from_water_value(v)),
        );
    } else if natural == Some("water") {
        push(
            Layer::Water,
            "natural",
            "water",
            FeatureKind::Water(WaterKind::Lake),
        );
    }
    if let Some(v) = natural.filter(|v| *v != "water") {
        push(
            Layer::Natural,
            "natural",
            v,
            FeatureKind::Natural(NaturalKind::from_tag_value(v)),
        );
    }

    simple!(Landuse, "landuse", Landuse, LanduseKind);
    if let Some(v) = kv.get(K::Building).filter(|v| *v != "no") {
        push(
            Layer::Building,
            "building",
            v,
            FeatureKind::Building(BuildingKind::from_tag_value(v)),
        );
    }
    simple!(Boundary, "boundary", Boundary, BoundaryKind);
    simple!(Place, "place", Place, PlaceKind);

    out
}

/// Whether a *closed* way classified as `c` describes an area (polygon)
/// rather than a closed line (ring road, fence, …).
///
/// `area=yes`/`area=no` override everything. Otherwise the key decides,
/// following the area-key conventions of iD and openstreetmap-carto: most
/// POI and land-cover keys imply an area, transport and waterway keys imply
/// a line except for a few area-like values.
pub fn closed_way_is_area(c: &Classified<'_>, area_tag: Option<&str>) -> bool {
    match area_tag {
        Some("yes") => return true,
        Some("no") => return false,
        _ => {}
    }
    match c.key {
        "highway" => matches!(c.value, "rest_area" | "services" | "platform"),
        "railway" => matches!(c.value, "platform" | "station" | "turntable" | "roundhouse"),
        "waterway" => matches!(c.value, "riverbank" | "dock" | "boatyard" | "dam" | "fuel"),
        "natural" => !matches!(
            c.value,
            "coastline"
                | "cliff"
                | "ridge"
                | "arete"
                | "tree_row"
                | "valley"
                | "gorge"
                | "earth_bank"
                | "dyke"
        ),
        "leisure" => !matches!(c.value, "track" | "slipway"),
        "historic" => c.value != "citywalls",
        // Boundaries are linear on ways; boundary areas come from relations.
        "boundary" => false,
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(tags: &[(&str, &str)]) -> Vec<FeatureKind> {
        let kv = KeyValues::scan(tags.iter().copied());
        classify(&kv, LayerSet::all())
            .iter()
            .map(|c| c.kind)
            .collect()
    }

    #[test]
    fn single_tag_classification() {
        assert_eq!(
            kinds(&[("highway", "residential")]),
            [FeatureKind::Highway(HighwayKind::Residential)]
        );
        assert_eq!(
            kinds(&[("building", "house")]),
            [FeatureKind::Building(BuildingKind::House)]
        );
        assert_eq!(
            kinds(&[("waterway", "river")]),
            [FeatureKind::Water(WaterKind::River)]
        );
        assert_eq!(
            kinds(&[("water", "lake")]),
            [FeatureKind::Water(WaterKind::Lake)]
        );
    }

    #[test]
    fn poi_and_building_are_both_kept() {
        assert_eq!(
            kinds(&[("building", "yes"), ("amenity", "school")]),
            [
                FeatureKind::Amenity(AmenityKind::School),
                FeatureKind::Building(BuildingKind::Yes)
            ]
        );
    }

    #[test]
    fn natural_water_goes_to_water_layer_only() {
        assert_eq!(
            kinds(&[("natural", "water")]),
            [FeatureKind::Water(WaterKind::Lake)]
        );
        assert_eq!(
            kinds(&[("natural", "water"), ("water", "reservoir")]),
            [FeatureKind::Water(WaterKind::Reservoir)]
        );
    }

    #[test]
    fn building_no_is_not_a_building() {
        assert!(kinds(&[("building", "no")]).is_empty());
        assert_eq!(
            kinds(&[("building", "no"), ("amenity", "parking")]),
            [FeatureKind::Amenity(AmenityKind::Parking)]
        );
    }

    #[test]
    fn empty_values_and_irrelevant_tags_are_ignored() {
        assert!(kinds(&[("name", "Main St"), ("maxspeed", "50")]).is_empty());
        assert!(kinds(&[("highway", "")]).is_empty());
        assert!(kinds(&[]).is_empty());
    }

    #[test]
    fn disabled_layers_are_skipped() {
        let kv = KeyValues::scan([("building", "yes"), ("amenity", "cafe")]);
        let only_buildings: LayerSet = [Layer::Building].into_iter().collect();
        let got: Vec<_> = classify(&kv, only_buildings)
            .iter()
            .map(|c| c.kind)
            .collect();
        assert_eq!(got, [FeatureKind::Building(BuildingKind::Yes)]);
    }

    #[test]
    fn unknown_values_keep_raw_value() {
        let kv = KeyValues::scan([("amenity", "bench")]);
        let c = classify(&kv, LayerSet::all())[0];
        assert_eq!(c.kind, FeatureKind::Amenity(AmenityKind::Other));
        assert_eq!((c.key, c.value), ("amenity", "bench"));
    }

    #[test]
    fn area_semantics() {
        let first = |tags: &[(&str, &str)]| {
            let kv = KeyValues::scan(tags.iter().copied());
            let c = classify(&kv, LayerSet::all())[0];
            closed_way_is_area(&c, kv.area())
        };
        // Area keys.
        assert!(first(&[("amenity", "school")]));
        assert!(first(&[("leisure", "playground")]));
        assert!(first(&[("shop", "mall")]));
        assert!(first(&[("building", "yes")]));
        assert!(first(&[("natural", "wood")]));
        // Line keys and exceptions.
        assert!(!first(&[("highway", "residential")]));
        assert!(!first(&[("natural", "coastline")]));
        assert!(!first(&[("leisure", "track")]));
        assert!(!first(&[("boundary", "administrative")]));
        assert!(first(&[("railway", "platform")]));
        // Explicit overrides.
        assert!(first(&[("highway", "pedestrian"), ("area", "yes")]));
        assert!(!first(&[("amenity", "parking"), ("area", "no")]));
    }
}
