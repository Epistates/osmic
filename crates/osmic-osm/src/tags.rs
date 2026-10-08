//! Compact tag storage: interned keys ([`TagStore`]), per-feature
//! [`Tags`], and the [`TagRetention`] policy deciding which tags are kept.

use std::sync::Arc;

use lasso::{Key, Spur, ThreadedRodeo};
use rustc_hash::{FxBuildHasher, FxHashMap};
use smallvec::SmallVec;
use smol_str::SmolStr;

/// Interned tag key (compact integer ID).
pub type TagKey = Spur;
/// Tag value, owned by its feature: stored inline up to 23 bytes, shared
/// (cheap to clone) beyond.
pub type TagValue = SmolStr;

/// Well-known OSM tag keys that appear millions of times.
/// Pre-interned for zero-cost matching in hot paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u16)]
pub enum WellKnownKey {
    /// `highway`.
    Highway = 0,
    /// `building`.
    Building,
    /// `name`.
    Name,
    /// `waterway`.
    Waterway,
    /// `natural`.
    Natural,
    /// `landuse`.
    Landuse,
    /// `railway`.
    Railway,
    /// `amenity`.
    Amenity,
    /// `leisure`.
    Leisure,
    /// `boundary`.
    Boundary,
    /// `place`.
    Place,
    /// `shop`.
    Shop,
    /// `tourism`.
    Tourism,
    /// `power`.
    Power,
    /// `aeroway`.
    Aeroway,
    /// `surface`.
    Surface,
    /// `maxspeed`.
    Maxspeed,
    /// `ref`.
    Ref,
    /// `oneway`.
    Oneway,
    /// `bridge`.
    Bridge,
    /// `tunnel`.
    Tunnel,
    /// `layer`.
    Layer,
    /// `access`.
    Access,
    /// `service`.
    Service,
    /// `foot`.
    Foot,
    /// `bicycle`.
    Bicycle,
    /// `lanes`.
    Lanes,
    /// `lit`.
    Lit,
    /// `admin_level`.
    AdminLevel,
    /// `water`.
    Water,
    /// `office`.
    Office,
    /// `healthcare`.
    Healthcare,
    /// `craft`.
    Craft,
    /// `historic`.
    Historic,
    /// `club`.
    Club,
    /// `emergency`.
    Emergency,
    /// `education`.
    Education,
    /// `addr:street`.
    AddrStreet,
    /// `addr:housenumber`.
    AddrHousenumber,
    /// `addr:city`.
    AddrCity,
    /// `addr:postcode`.
    AddrPostcode,
    /// `phone`.
    Phone,
    /// `contact:phone`.
    ContactPhone,
    /// `website`.
    Website,
    /// `contact:website`.
    ContactWebsite,
    /// `opening_hours`.
    OpeningHours,
    /// `cuisine`.
    Cuisine,
    /// `brand`.
    Brand,
    /// `operator`.
    Operator,
    /// `description`.
    Description,
}

impl WellKnownKey {
    /// Every variant, in discriminant order.
    pub const ALL: &[WellKnownKey] = &[
        Self::Highway,
        Self::Building,
        Self::Name,
        Self::Waterway,
        Self::Natural,
        Self::Landuse,
        Self::Railway,
        Self::Amenity,
        Self::Leisure,
        Self::Boundary,
        Self::Place,
        Self::Shop,
        Self::Tourism,
        Self::Power,
        Self::Aeroway,
        Self::Surface,
        Self::Maxspeed,
        Self::Ref,
        Self::Oneway,
        Self::Bridge,
        Self::Tunnel,
        Self::Layer,
        Self::Access,
        Self::Service,
        Self::Foot,
        Self::Bicycle,
        Self::Lanes,
        Self::Lit,
        Self::AdminLevel,
        Self::Water,
        Self::Office,
        Self::Healthcare,
        Self::Craft,
        Self::Historic,
        Self::Club,
        Self::Emergency,
        Self::Education,
        Self::AddrStreet,
        Self::AddrHousenumber,
        Self::AddrCity,
        Self::AddrPostcode,
        Self::Phone,
        Self::ContactPhone,
        Self::Website,
        Self::ContactWebsite,
        Self::OpeningHours,
        Self::Cuisine,
        Self::Brand,
        Self::Operator,
        Self::Description,
    ];

    /// The OSM key string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Highway => "highway",
            Self::Building => "building",
            Self::Name => "name",
            Self::Waterway => "waterway",
            Self::Natural => "natural",
            Self::Landuse => "landuse",
            Self::Railway => "railway",
            Self::Amenity => "amenity",
            Self::Leisure => "leisure",
            Self::Boundary => "boundary",
            Self::Place => "place",
            Self::Shop => "shop",
            Self::Tourism => "tourism",
            Self::Power => "power",
            Self::Aeroway => "aeroway",
            Self::Surface => "surface",
            Self::Maxspeed => "maxspeed",
            Self::Ref => "ref",
            Self::Oneway => "oneway",
            Self::Bridge => "bridge",
            Self::Tunnel => "tunnel",
            Self::Layer => "layer",
            Self::Access => "access",
            Self::Service => "service",
            Self::Foot => "foot",
            Self::Bicycle => "bicycle",
            Self::Lanes => "lanes",
            Self::Lit => "lit",
            Self::AdminLevel => "admin_level",
            Self::Water => "water",
            Self::Office => "office",
            Self::Healthcare => "healthcare",
            Self::Craft => "craft",
            Self::Historic => "historic",
            Self::Club => "club",
            Self::Emergency => "emergency",
            Self::Education => "education",
            Self::AddrStreet => "addr:street",
            Self::AddrHousenumber => "addr:housenumber",
            Self::AddrCity => "addr:city",
            Self::AddrPostcode => "addr:postcode",
            Self::Phone => "phone",
            Self::ContactPhone => "contact:phone",
            Self::Website => "website",
            Self::ContactWebsite => "contact:website",
            Self::OpeningHours => "opening_hours",
            Self::Cuisine => "cuisine",
            Self::Brand => "brand",
            Self::Operator => "operator",
            Self::Description => "description",
        }
    }
}

/// Interner for tag keys, shared by the threads of a pipeline.
///
/// Keys repeat across millions of elements, so each is stored once and
/// features carry a 4-byte [`TagKey`]. The well-known and curated keys are
/// interned up front and found and resolved without locking; other keys
/// (with [`TagRetention::All`]) go through a concurrent interner. Values
/// are mostly unique (names, addresses) and are owned by each feature's
/// [`Tags`] instead, so they are freed with the feature.
pub struct TagStore {
    rodeo: ThreadedRodeo<Spur, FxBuildHasher>,
    /// Keys interned at construction, by string.
    preset: FxHashMap<&'static str, TagKey>,
    /// Keys interned at construction, by key index.
    preset_names: Vec<&'static str>,
    well_known: Vec<TagKey>,
}

impl TagStore {
    /// A store with every [`WellKnownKey`] and [`CURATED_KEYS`] entry
    /// already interned.
    pub fn new() -> Self {
        let rodeo: ThreadedRodeo<TagKey, FxBuildHasher> = ThreadedRodeo::with_hasher(FxBuildHasher);
        let mut preset = FxHashMap::default();
        let mut preset_names = Vec::new();
        let names = WellKnownKey::ALL
            .iter()
            .map(|wk| wk.as_str())
            .chain(CURATED_KEYS.iter().copied());
        for name in names {
            let key = rodeo.get_or_intern_static(name);
            if preset.insert(name, key).is_none() {
                debug_assert_eq!(key.into_usize(), preset_names.len());
                preset_names.push(name);
            }
        }
        let well_known = WellKnownKey::ALL
            .iter()
            .map(|wk| preset[wk.as_str()])
            .collect();
        Self {
            rodeo,
            preset,
            preset_names,
            well_known,
        }
    }

    /// Intern a tag key string, returning its compact ID.
    pub fn intern_key(&self, key: &str) -> TagKey {
        match self.preset.get(key) {
            Some(&k) => k,
            None => self.rodeo.get_or_intern(key),
        }
    }

    /// The string of an interned key.
    pub fn resolve(&self, key: TagKey) -> &str {
        match self.preset_names.get(key.into_usize()) {
            Some(name) => name,
            None => self.rodeo.resolve(&key),
        }
    }

    /// Get the pre-interned key for a well-known OSM tag.
    pub fn well_known(&self, wk: WellKnownKey) -> TagKey {
        self.well_known[wk as usize]
    }

    /// Look up a key without interning it.
    pub fn get(&self, key: &str) -> Option<TagKey> {
        self.preset
            .get(key)
            .copied()
            .or_else(|| self.rodeo.get(key))
    }

    /// Number of distinct keys interned, including the preset keys.
    pub fn len(&self) -> usize {
        self.rodeo.len()
    }

    /// Whether no key is interned; never true for a store built by
    /// [`TagStore::new`], which presets keys.
    pub fn is_empty(&self) -> bool {
        self.rodeo.is_empty()
    }
}

impl Default for TagStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Compact tag storage for a single OSM element.
///
/// Uses `SmallVec` with inline capacity of 4, since most OSM elements
/// have 0-5 tags. Avoids heap allocation for the common case.
#[derive(Debug, Clone)]
pub struct Tags {
    inner: SmallVec<[(TagKey, TagValue); 4]>,
}

impl Tags {
    /// No tags; does not allocate.
    pub fn new() -> Self {
        Self {
            inner: SmallVec::new(),
        }
    }

    /// Room for `cap` tags; allocates only beyond the inline capacity of 4.
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            inner: SmallVec::with_capacity(cap),
        }
    }

    /// Append a tag. Duplicate keys are not checked; [`Tags::get`] returns
    /// the first.
    pub fn push(&mut self, key: TagKey, value: impl Into<TagValue>) {
        self.inner.push((key, value.into()));
    }

    /// Look up a tag value by key. Linear scan (fast for small N).
    pub fn get(&self, key: TagKey) -> Option<&str> {
        self.inner
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.as_str())
    }

    /// Whether a tag with `key` is present.
    pub fn contains(&self, key: TagKey) -> bool {
        self.inner.iter().any(|(k, _)| *k == key)
    }

    /// Tags in insertion order; resolve keys with [`TagStore::resolve`].
    pub fn iter(&self) -> impl Iterator<Item = &(TagKey, TagValue)> {
        self.inner.iter()
    }

    /// Number of tags.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Whether there are no tags.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

impl Default for Tags {
    fn default() -> Self {
        Self::new()
    }
}

/// Tag keys kept by [`TagRetention::Curated`]: every classification key
/// (so a feature's raw class value is available), names, references and
/// the address/contact fields written into vector tiles.
pub const CURATED_KEYS: &[&str] = &[
    // Classification keys.
    "highway",
    "building",
    "waterway",
    "water",
    "natural",
    "landuse",
    "railway",
    "amenity",
    "leisure",
    "boundary",
    "place",
    "shop",
    "tourism",
    "office",
    "healthcare",
    "craft",
    "historic",
    "club",
    "emergency",
    "education",
    // Identity and labelling.
    "name",
    "ref",
    "admin_level",
    // Address and contact.
    "addr:street",
    "addr:housenumber",
    "addr:city",
    "addr:postcode",
    "phone",
    "contact:phone",
    "website",
    "contact:website",
    "opening_hours",
    "cuisine",
    "brand",
    "operator",
    "description",
];

/// Which tags a pipeline keeps on the features it produces.
///
/// Keeping only what the output needs bounds per-feature memory and the
/// number of distinct keys interned.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub enum TagRetention {
    /// Keep every tag.
    #[default]
    All,
    /// Keep only [`CURATED_KEYS`].
    Curated,
    /// Keep only the listed keys.
    Keys(Arc<[String]>),
}

impl TagRetention {
    /// Whether tags with this key are kept.
    pub fn keeps(&self, key: &str) -> bool {
        match self {
            Self::All => true,
            Self::Curated => CURATED_KEYS.contains(&key),
            Self::Keys(keys) => keys.iter().any(|k| k == key),
        }
    }
}

impl TagStore {
    /// Keep the retained subset of `tags`, interning their keys.
    pub fn intern_tags<'a>(
        &self,
        tags: impl IntoIterator<Item = (&'a str, &'a str)>,
        retention: &TagRetention,
    ) -> Tags {
        let mut out = Tags::new();
        for (k, v) in tags {
            if retention.keeps(k) {
                out.push(self.intern_key(k), v);
            }
        }
        out
    }

    /// Every tag of `tags` as string slices.
    pub fn resolve_tags<'a>(&'a self, tags: &'a Tags) -> impl Iterator<Item = (&'a str, &'a str)> {
        tags.iter().map(|(k, v)| (self.resolve(*k), v.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- TagStore: well_known resolves to the correct string ---

    #[test]
    fn well_known_resolves_to_correct_string() {
        let store = TagStore::new();
        for wk in WellKnownKey::ALL {
            let key = store.well_known(*wk);
            let resolved = store.resolve(key);
            assert_eq!(
                resolved,
                wk.as_str(),
                "WellKnownKey::{:?} resolved to {:?}, expected {:?}",
                wk,
                resolved,
                wk.as_str()
            );
        }
    }

    #[test]
    fn well_known_highway_key_is_interned_before_custom_keys() {
        let store = TagStore::new();
        // Interning "highway" again must return the same Spur as well_known(Highway).
        let via_intern = store.intern_key("highway");
        let via_well_known = store.well_known(WellKnownKey::Highway);
        assert_eq!(via_intern, via_well_known);
    }

    // --- intern_key / resolve round-trip ---

    #[test]
    fn intern_key_resolve_roundtrip() {
        let store = TagStore::new();
        let key = store.intern_key("custom_key");
        assert_eq!(store.resolve(key), "custom_key");
    }

    #[test]
    fn curated_keys_resolve_without_the_interner() {
        let store = TagStore::new();
        let before = store.len();
        for key in CURATED_KEYS {
            let k = store.intern_key(key);
            assert_eq!(store.resolve(k), *key);
            assert_eq!(store.get(key), Some(k));
        }
        assert_eq!(store.len(), before, "no new keys interned");
    }

    #[test]
    fn intern_same_string_twice_returns_same_spur() {
        let store = TagStore::new();
        let a = store.intern_key("duplicate");
        let b = store.intern_key("duplicate");
        assert_eq!(a, b);
    }

    #[test]
    fn get_returns_none_for_missing_key() {
        let store = TagStore::new();
        assert!(store.get("this_key_was_never_interned_xyz").is_none());
    }

    #[test]
    fn get_returns_some_after_interning() {
        let store = TagStore::new();
        store.intern_key("present");
        assert!(store.get("present").is_some());
    }

    // --- Tags: get / contains / len ---

    #[test]
    fn tags_get_and_contains() {
        let store = TagStore::new();
        let highway_key = store.well_known(WellKnownKey::Highway);

        let mut tags = Tags::new();
        assert_eq!(tags.len(), 0);
        assert!(tags.is_empty());
        assert!(!tags.contains(highway_key));
        assert!(tags.get(highway_key).is_none());

        tags.push(highway_key, "motorway");
        assert_eq!(tags.len(), 1);
        assert!(!tags.is_empty());
        assert!(tags.contains(highway_key));
        assert_eq!(tags.get(highway_key), Some("motorway"));
    }

    #[test]
    fn tags_get_absent_key_returns_none() {
        let store = TagStore::new();
        let building_key = store.well_known(WellKnownKey::Building);
        let name_key = store.well_known(WellKnownKey::Name);

        let mut tags = Tags::new();
        tags.push(building_key, "yes");

        // name was never pushed.
        assert!(tags.get(name_key).is_none());
        assert!(!tags.contains(name_key));
    }

    #[test]
    fn retention_filters_interned_tags() {
        let store = TagStore::new();
        let raw = [
            ("name", "Cafe"),
            ("amenity", "cafe"),
            ("fixme", "check"),
            ("note", "x"),
        ];
        let all = store.intern_tags(raw, &TagRetention::All);
        let curated = store.intern_tags(raw, &TagRetention::Curated);
        let custom = store.intern_tags(raw, &TagRetention::Keys(vec!["note".to_string()].into()));
        assert_eq!(all.len(), 4);
        assert_eq!(curated.len(), 2);
        assert_eq!(custom.len(), 1);
        let resolved: Vec<_> = store.resolve_tags(&curated).collect();
        assert_eq!(resolved, [("name", "Cafe"), ("amenity", "cafe")]);
        // Nothing outside the retained set was interned by the curated pass.
        let fresh = TagStore::new();
        fresh.intern_tags(raw, &TagRetention::Curated);
        assert!(fresh.get("fixme").is_none());
    }

    // --- Tags: capacity exceeding inline SmallVec ---

    #[test]
    fn tags_exceeds_inline_capacity() {
        let store = TagStore::new();
        // The inline capacity is 4; push more than 4 pairs to force heap allocation.
        let mut tags = Tags::with_capacity(8);
        let keys: Vec<TagKey> = WellKnownKey::ALL
            .iter()
            .take(8)
            .map(|wk| store.well_known(*wk))
            .collect();
        for &k in &keys {
            tags.push(k, "test");
        }

        assert_eq!(tags.len(), 8);

        // Every pushed key must be retrievable.
        for &k in &keys {
            assert!(tags.contains(k));
            assert_eq!(tags.get(k), Some("test"));
        }
    }
}
