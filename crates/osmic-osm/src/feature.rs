use osmic_core::{Geometry, OsmId};
use serde::{Deserialize, Serialize};

use crate::layers::Layer;
use crate::tags::Tags;

/// Declares a tag-value enum with an `Other` catch-all, `from_tag_value`
/// (first literal plus any `|` aliases) and `as_str` (first literal).
macro_rules! tag_value_enum {
    (
        $(#[$meta:meta])*
        $name:ident { $($variant:ident => $value:literal $(| $alias:literal)*),* $(,)? }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub enum $name {
            $($variant,)*
            /// Any value without a dedicated variant.
            Other,
        }

        impl $name {
            /// Every named variant (excluding `Other`).
            pub const NAMED: &'static [Self] = &[$(Self::$variant),*];

            pub fn from_tag_value(val: &str) -> Self {
                match val {
                    $($value $(| $alias)* => Self::$variant,)*
                    _ => Self::Other,
                }
            }

            pub const fn as_str(&self) -> &'static str {
                match self {
                    $(Self::$variant => $value,)*
                    Self::Other => "other",
                }
            }
        }
    };
}

tag_value_enum!(
    /// `highway=*` values.
    HighwayKind {
        Motorway => "motorway",
        MotorwayLink => "motorway_link",
        Trunk => "trunk",
        TrunkLink => "trunk_link",
        Primary => "primary",
        PrimaryLink => "primary_link",
        Secondary => "secondary",
        SecondaryLink => "secondary_link",
        Tertiary => "tertiary",
        TertiaryLink => "tertiary_link",
        Residential => "residential",
        Unclassified => "unclassified",
        Service => "service",
        LivingStreet => "living_street",
        Pedestrian => "pedestrian",
        Track => "track",
        BusGuideway => "bus_guideway",
        Footway => "footway",
        Bridleway => "bridleway",
        Steps => "steps",
        Corridor => "corridor",
        Path => "path",
        Cycleway => "cycleway",
    }
);

tag_value_enum!(
    /// `building=*` values.
    BuildingKind {
        Yes => "yes",
        House => "house",
        Apartments => "apartments",
        Commercial => "commercial",
        Industrial => "industrial",
        Retail => "retail",
        Garage => "garage",
        Garages => "garages",
        Shed => "shed",
        Hut => "hut",
        Cabin => "cabin",
        Church => "church",
        Cathedral => "cathedral",
        Mosque => "mosque",
        Temple => "temple",
        Synagogue => "synagogue",
        Hospital => "hospital",
        School => "school",
        University => "university",
        Kindergarten => "kindergarten",
        Hotel => "hotel",
        Office => "office",
    }
);

/// Water features from `waterway=*`, `water=*` and `natural=water`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum WaterKind {
    River,
    Stream,
    Canal,
    Drain,
    Ditch,
    Lake,
    Pond,
    Reservoir,
    Basin,
    Wetland,
    Coastline,
    Other,
}

impl WaterKind {
    pub fn from_waterway_value(val: &str) -> Self {
        match val {
            "river" => Self::River,
            "stream" => Self::Stream,
            "canal" => Self::Canal,
            "drain" => Self::Drain,
            "ditch" => Self::Ditch,
            _ => Self::Other,
        }
    }

    pub fn from_water_value(val: &str) -> Self {
        match val {
            "lake" => Self::Lake,
            "pond" => Self::Pond,
            "reservoir" => Self::Reservoir,
            "basin" => Self::Basin,
            "river" => Self::River,
            _ => Self::Other,
        }
    }

    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::River => "river",
            Self::Stream => "stream",
            Self::Canal => "canal",
            Self::Drain => "drain",
            Self::Ditch => "ditch",
            Self::Lake => "lake",
            Self::Pond => "pond",
            Self::Reservoir => "reservoir",
            Self::Basin => "basin",
            Self::Wetland => "wetland",
            Self::Coastline => "coastline",
            Self::Other => "other",
        }
    }
}

tag_value_enum!(
    /// `landuse=*` values.
    LanduseKind {
        Residential => "residential",
        Commercial => "commercial",
        Industrial => "industrial",
        Retail => "retail",
        Farmland => "farmland" | "farm" | "farmyard",
        Forest => "forest",
        Grass => "grass",
        Meadow => "meadow",
        Orchard => "orchard",
        Vineyard => "vineyard",
        Cemetery => "cemetery",
        Military => "military",
        Quarry => "quarry",
        Recreation => "recreation_ground",
    }
);

tag_value_enum!(
    /// `natural=*` values.
    NaturalKind {
        Water => "water",
        Wood => "wood",
        Scrub => "scrub",
        Grassland => "grassland",
        Heath => "heath",
        Sand => "sand",
        Bare => "bare_rock",
        Wetland => "wetland",
        Glacier => "glacier",
        Beach => "beach",
        Cliff => "cliff",
        Peak => "peak",
        Volcano => "volcano",
        Tree => "tree",
        Coastline => "coastline",
    }
);

tag_value_enum!(
    /// `railway=*` values.
    RailwayKind {
        Rail => "rail",
        Subway => "subway",
        Tram => "tram",
        LightRail => "light_rail",
        Monorail => "monorail",
        Narrow => "narrow_gauge",
        Preserved => "preserved",
        Disused => "disused",
        Abandoned => "abandoned",
        Platform => "platform",
        Station => "station",
    }
);

tag_value_enum!(
    /// `amenity=*` values.
    AmenityKind {
        Parking => "parking",
        School => "school",
        PlaceOfWorship => "place_of_worship",
        Restaurant => "restaurant",
        Fuel => "fuel",
        Hospital => "hospital",
        Pharmacy => "pharmacy",
        Bank => "bank",
        Cafe => "cafe",
        FastFood => "fast_food",
        Pub => "pub",
        Bar => "bar",
        Police => "police",
        FireStation => "fire_station",
        PostOffice => "post_office",
        Library => "library",
        University => "university",
        Kindergarten => "kindergarten",
        Marketplace => "marketplace",
    }
);

tag_value_enum!(
    /// `leisure=*` values.
    LeisureKind {
        Park => "park",
        Garden => "garden",
        Playground => "playground",
        GolfCourse => "golf_course",
        SportsCentre => "sports_centre",
        SwimmingPool => "swimming_pool",
        Stadium => "stadium",
        Pitch => "pitch",
        NatureReserve => "nature_reserve",
        Marina => "marina",
    }
);

tag_value_enum!(
    /// `shop=*` values.
    ShopKind {
        Supermarket => "supermarket",
        Convenience => "convenience",
        Clothes => "clothes",
        Hairdresser => "hairdresser",
        CarRepair => "car_repair",
        Bakery => "bakery",
        Beauty => "beauty",
        Car => "car",
        MobilePhone => "mobile_phone",
        Hardware => "hardware",
        Butcher => "butcher",
        Alcohol => "alcohol",
        Furniture => "furniture",
        Electronics => "electronics",
        DepartmentStore => "department_store",
        Mall => "mall",
        Bicycle => "bicycle",
        Books => "books",
        Jewelry => "jewelry",
        Gift => "gift",
        Florist => "florist",
        Pet => "pet",
        Sports => "sports",
        Optician => "optician",
    }
);

tag_value_enum!(
    /// `tourism=*` values.
    TourismKind {
        Hotel => "hotel",
        Motel => "motel",
        Attraction => "attraction",
        Museum => "museum",
        Viewpoint => "viewpoint",
        Information => "information",
        GuestHouse => "guest_house",
        CampSite => "camp_site",
        PicnicSite => "picnic_site",
        ThemePark => "theme_park",
        Zoo => "zoo",
        Hostel => "hostel",
        Artwork => "artwork",
    }
);

tag_value_enum!(
    /// `office=*` values.
    OfficeKind {
        Company => "company",
        Government => "government",
        Insurance => "insurance",
        Lawyer => "lawyer",
        EstateAgent => "estate_agent",
        Financial => "financial",
        It => "it",
        Ngo => "ngo",
        Accountant => "accountant",
        Architect => "architect",
    }
);

tag_value_enum!(
    /// `healthcare=*` values.
    HealthcareKind {
        Doctor => "doctor",
        Dentist => "dentist",
        Clinic => "clinic",
        Hospital => "hospital",
        Pharmacy => "pharmacy",
        Optometrist => "optometrist",
        Physiotherapist => "physiotherapist",
        Laboratory => "laboratory",
        Rehabilitation => "rehabilitation",
    }
);

tag_value_enum!(
    /// `craft=*` values.
    CraftKind {
        Carpenter => "carpenter",
        Electrician => "electrician",
        Plumber => "plumber",
        Painter => "painter",
        Brewery => "brewery",
        Photographer => "photographer",
        Tailor => "tailor",
        Hvac => "hvac",
        Shoemaker => "shoemaker",
        Gardener => "gardener",
        Locksmith => "locksmith",
        Roofer => "roofer",
    }
);

tag_value_enum!(
    /// `historic=*` values.
    HistoricKind {
        Monument => "monument",
        Memorial => "memorial",
        Castle => "castle",
        Ruins => "ruins",
        ArchaeologicalSite => "archaeological_site",
        Fort => "fort",
        Battlefield => "battlefield",
        Building => "building",
    }
);

tag_value_enum!(
    /// `club=*` values.
    ClubKind {
        Sport => "sport",
        Social => "social",
        Veterans => "veterans",
        Music => "music",
        Gaming => "gaming",
        Fishing => "fishing",
    }
);

tag_value_enum!(
    /// `emergency=*` values.
    EmergencyKind {
        AmbulanceStation => "ambulance_station",
        FireStation => "fire_station",
        FireHydrant => "fire_hydrant",
        Hospital => "hospital",
        Phone => "phone",
        Defibrillator => "defibrillator",
        AssemblyPoint => "assembly_point",
    }
);

tag_value_enum!(
    /// `education=*` values.
    EducationKind {
        School => "school",
        University => "university",
        College => "college",
        Kindergarten => "kindergarten",
        LanguageSchool => "language_school",
        DrivingSchool => "driving_school",
        MusicSchool => "music_school",
    }
);

tag_value_enum!(
    /// `boundary=*` values.
    BoundaryKind {
        Administrative => "administrative",
        NationalPark => "national_park",
        Protected => "protected_area",
        Maritime => "maritime",
    }
);

tag_value_enum!(
    /// `place=*` values.
    PlaceKind {
        City => "city",
        Town => "town",
        Village => "village",
        Hamlet => "hamlet",
        Suburb => "suburb",
        Neighbourhood => "neighbourhood",
        IsolatedDwelling => "isolated_dwelling",
    }
);

/// Top-level feature classification from OSM tags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FeatureKind {
    Highway(HighwayKind),
    Building(BuildingKind),
    Water(WaterKind),
    Landuse(LanduseKind),
    Natural(NaturalKind),
    Railway(RailwayKind),
    Amenity(AmenityKind),
    Leisure(LeisureKind),
    Shop(ShopKind),
    Tourism(TourismKind),
    Office(OfficeKind),
    Healthcare(HealthcareKind),
    Craft(CraftKind),
    Historic(HistoricKind),
    Club(ClubKind),
    Emergency(EmergencyKind),
    Education(EducationKind),
    Boundary(BoundaryKind),
    Place(PlaceKind),
}

impl FeatureKind {
    /// The layer this kind belongs to.
    pub const fn layer(&self) -> Layer {
        match self {
            Self::Highway(_) => Layer::Highway,
            Self::Building(_) => Layer::Building,
            Self::Water(_) => Layer::Water,
            Self::Landuse(_) => Layer::Landuse,
            Self::Natural(_) => Layer::Natural,
            Self::Railway(_) => Layer::Railway,
            Self::Amenity(_) => Layer::Amenity,
            Self::Leisure(_) => Layer::Leisure,
            Self::Shop(_) => Layer::Shop,
            Self::Tourism(_) => Layer::Tourism,
            Self::Office(_) => Layer::Office,
            Self::Healthcare(_) => Layer::Healthcare,
            Self::Craft(_) => Layer::Craft,
            Self::Historic(_) => Layer::Historic,
            Self::Club(_) => Layer::Club,
            Self::Emergency(_) => Layer::Emergency,
            Self::Education(_) => Layer::Education,
            Self::Boundary(_) => Layer::Boundary,
            Self::Place(_) => Layer::Place,
        }
    }

    /// Vector-tile layer name for this kind.
    pub const fn layer_name(&self) -> &'static str {
        self.layer().as_str()
    }

    /// Subtype name of the known variant (`"other"` for unlisted values;
    /// the tile encoder emits the raw OSM value instead in that case).
    pub const fn class_name(&self) -> &'static str {
        match self {
            Self::Highway(k) => k.as_str(),
            Self::Building(k) => k.as_str(),
            Self::Water(k) => k.as_str(),
            Self::Landuse(k) => k.as_str(),
            Self::Natural(k) => k.as_str(),
            Self::Railway(k) => k.as_str(),
            Self::Amenity(k) => k.as_str(),
            Self::Leisure(k) => k.as_str(),
            Self::Shop(k) => k.as_str(),
            Self::Tourism(k) => k.as_str(),
            Self::Office(k) => k.as_str(),
            Self::Healthcare(k) => k.as_str(),
            Self::Craft(k) => k.as_str(),
            Self::Historic(k) => k.as_str(),
            Self::Club(k) => k.as_str(),
            Self::Emergency(k) => k.as_str(),
            Self::Education(k) => k.as_str(),
            Self::Boundary(k) => k.as_str(),
            Self::Place(k) => k.as_str(),
        }
    }

    /// Minimum zoom level at which this feature should appear in tiles.
    pub fn min_zoom(&self) -> u8 {
        match self {
            Self::Highway(h) => match h {
                HighwayKind::Motorway | HighwayKind::MotorwayLink => 4,
                HighwayKind::Trunk | HighwayKind::TrunkLink => 5,
                HighwayKind::Primary | HighwayKind::PrimaryLink => 7,
                HighwayKind::Secondary | HighwayKind::SecondaryLink => 9,
                HighwayKind::Tertiary | HighwayKind::TertiaryLink => 11,
                HighwayKind::Residential
                | HighwayKind::Unclassified
                | HighwayKind::LivingStreet => 12,
                HighwayKind::Service | HighwayKind::Pedestrian => 13,
                _ => 14,
            },
            Self::Building(_) => 13,
            Self::Water(w) => match w {
                WaterKind::Coastline => 0,
                WaterKind::Lake | WaterKind::Reservoir => 6,
                WaterKind::River => 8,
                WaterKind::Pond | WaterKind::Basin => 10,
                WaterKind::Stream | WaterKind::Canal => 12,
                _ => 13,
            },
            Self::Landuse(_) => 7,
            Self::Natural(n) => match n {
                NaturalKind::Coastline => 0,
                NaturalKind::Water | NaturalKind::Wood | NaturalKind::Glacier => 6,
                _ => 10,
            },
            Self::Railway(r) => match r {
                RailwayKind::Rail => 8,
                RailwayKind::Subway | RailwayKind::LightRail => 10,
                _ => 12,
            },
            Self::Amenity(_) => 13,
            Self::Shop(_) | Self::Office(_) | Self::Healthcare(_) | Self::Craft(_) => 14,
            Self::Tourism(t) => match t {
                TourismKind::ThemePark | TourismKind::Zoo => 10,
                TourismKind::Hotel | TourismKind::Museum | TourismKind::Attraction => 13,
                _ => 14,
            },
            Self::Historic(h) => match h {
                HistoricKind::Castle | HistoricKind::Fort => 10,
                _ => 13,
            },
            Self::Club(_) => 14,
            Self::Emergency(_) | Self::Education(_) => 13,
            Self::Leisure(l) => match l {
                LeisureKind::Park | LeisureKind::NatureReserve => 8,
                LeisureKind::GolfCourse | LeisureKind::Stadium => 10,
                _ => 12,
            },
            Self::Boundary(_) => 2,
            Self::Place(p) => match p {
                PlaceKind::City => 4,
                PlaceKind::Town => 7,
                PlaceKind::Village => 10,
                _ => 12,
            },
        }
    }

    /// Relative importance within a tile (higher = more important), used to
    /// order features and to decide which to drop first when a tile exceeds
    /// its size budget. Lower `min_zoom` means more important.
    pub fn importance(&self) -> u8 {
        let base = 32u8.saturating_sub(self.min_zoom()) * 4;
        // Labels and transport outrank generic area fill at the same zoom.
        let bonus = match self.layer() {
            Layer::Place => 3,
            Layer::Highway | Layer::Railway | Layer::Water => 2,
            Layer::Boundary => 1,
            _ => 0,
        };
        base + bonus
    }
}

/// A classified OSM feature with geometry and tags.
#[derive(Debug, Clone)]
pub struct Feature {
    /// The OSM element this feature came from.
    pub id: OsmId,
    pub kind: FeatureKind,
    /// WGS84 geometry (x = longitude, y = latitude). Polygon exteriors are
    /// counter-clockwise and holes clockwise.
    pub geometry: Geometry,
    pub tags: Tags,
}

impl Feature {
    /// Bounding box of this feature's geometry.
    pub fn bbox(&self) -> osmic_core::BBox {
        self.geometry.bbox()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn as_str_round_trips_for_every_named_variant() {
        macro_rules! check {
            ($($t:ty),*) => {$(
                for k in <$t>::NAMED {
                    assert_eq!(<$t>::from_tag_value(k.as_str()), *k, "{k:?}");
                }
                assert_eq!(<$t>::from_tag_value("definitely_not_a_value"), <$t>::Other);
            )*};
        }
        check!(
            HighwayKind,
            BuildingKind,
            LanduseKind,
            NaturalKind,
            RailwayKind,
            AmenityKind,
            LeisureKind,
            ShopKind,
            TourismKind,
            OfficeKind,
            HealthcareKind,
            CraftKind,
            HistoricKind,
            ClubKind,
            EmergencyKind,
            EducationKind,
            BoundaryKind,
            PlaceKind
        );
    }

    #[test]
    fn aliases_map_to_canonical_variant() {
        assert_eq!(LanduseKind::from_tag_value("farm"), LanduseKind::Farmland);
        assert_eq!(LanduseKind::Farmland.as_str(), "farmland");
    }

    #[test]
    fn fire_hydrant_is_not_a_fire_station() {
        assert_eq!(
            EmergencyKind::from_tag_value("fire_hydrant"),
            EmergencyKind::FireHydrant
        );
    }

    #[test]
    fn min_zoom_orders_road_hierarchy() {
        let z = |h| FeatureKind::Highway(h).min_zoom();
        assert_eq!(z(HighwayKind::Motorway), 4);
        assert!(z(HighwayKind::Motorway) < z(HighwayKind::Primary));
        assert!(z(HighwayKind::Primary) < z(HighwayKind::Residential));
        assert_eq!(z(HighwayKind::Residential), 12);
    }

    #[test]
    fn importance_prefers_low_min_zoom() {
        let motorway = FeatureKind::Highway(HighwayKind::Motorway).importance();
        let footway = FeatureKind::Highway(HighwayKind::Footway).importance();
        let shop = FeatureKind::Shop(ShopKind::Bakery).importance();
        assert!(motorway > footway);
        assert!(footway > shop);
    }

    #[test]
    fn layer_names() {
        assert_eq!(
            FeatureKind::Highway(HighwayKind::Motorway).layer_name(),
            "highway"
        );
        assert_eq!(FeatureKind::Place(PlaceKind::City).layer_name(), "place");
    }
}
