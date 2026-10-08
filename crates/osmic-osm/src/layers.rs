use std::fmt;
use std::str::FromStr;

/// A feature layer. Each classified feature belongs to exactly one layer,
/// which becomes its vector-tile layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum Layer {
    Highway,
    Building,
    Water,
    Natural,
    Landuse,
    Railway,
    Amenity,
    Leisure,
    Boundary,
    Place,
    Shop,
    Tourism,
    Office,
    Healthcare,
    Craft,
    Historic,
    Club,
    Emergency,
    Education,
}

impl Layer {
    /// Every layer, in a stable order.
    pub const ALL: [Layer; 19] = [
        Self::Highway,
        Self::Building,
        Self::Water,
        Self::Natural,
        Self::Landuse,
        Self::Railway,
        Self::Amenity,
        Self::Leisure,
        Self::Boundary,
        Self::Place,
        Self::Shop,
        Self::Tourism,
        Self::Office,
        Self::Healthcare,
        Self::Craft,
        Self::Historic,
        Self::Club,
        Self::Emergency,
        Self::Education,
    ];

    /// The layer's name (also its vector-tile layer id).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Highway => "highway",
            Self::Building => "building",
            Self::Water => "water",
            Self::Natural => "natural",
            Self::Landuse => "landuse",
            Self::Railway => "railway",
            Self::Amenity => "amenity",
            Self::Leisure => "leisure",
            Self::Boundary => "boundary",
            Self::Place => "place",
            Self::Shop => "shop",
            Self::Tourism => "tourism",
            Self::Office => "office",
            Self::Healthcare => "healthcare",
            Self::Craft => "craft",
            Self::Historic => "historic",
            Self::Club => "club",
            Self::Emergency => "emergency",
            Self::Education => "education",
        }
    }

    const fn bit(self) -> u32 {
        1 << self as u8
    }
}

impl fmt::Display for Layer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Error for an unrecognised layer name.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown layer '{name}' (available: {available})")]
pub struct UnknownLayer {
    pub name: String,
    available: String,
}

impl FromStr for Layer {
    type Err = UnknownLayer;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|l| l.as_str() == s)
            .ok_or_else(|| UnknownLayer {
                name: s.to_string(),
                available: LayerSet::all().to_string(),
            })
    }
}

/// A set of enabled layers (bitset).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LayerSet(u32);

impl LayerSet {
    /// Every layer.
    pub const fn all() -> Self {
        Self((1 << Layer::ALL.len()) - 1)
    }

    /// No layers.
    pub const fn none() -> Self {
        Self(0)
    }

    pub const fn contains(self, layer: Layer) -> bool {
        self.0 & layer.bit() != 0
    }

    pub fn insert(&mut self, layer: Layer) {
        self.0 |= layer.bit();
    }

    pub fn remove(&mut self, layer: Layer) {
        self.0 &= !layer.bit();
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Enabled layers in [`Layer::ALL`] order.
    pub fn iter(self) -> impl Iterator<Item = Layer> {
        Layer::ALL.into_iter().filter(move |l| self.contains(*l))
    }

    /// Parse a comma-separated list of layer names (whitespace ignored).
    pub fn from_names(input: &str) -> Result<Self, UnknownLayer> {
        let mut set = Self::none();
        for name in input.split(',').map(str::trim).filter(|n| !n.is_empty()) {
            set.insert(name.parse()?);
        }
        Ok(set)
    }
}

impl Default for LayerSet {
    fn default() -> Self {
        Self::all()
    }
}

impl FromIterator<Layer> for LayerSet {
    fn from_iter<I: IntoIterator<Item = Layer>>(iter: I) -> Self {
        let mut set = Self::none();
        for l in iter {
            set.insert(l);
        }
        set
    }
}

impl fmt::Display for LayerSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for l in self.iter() {
            if !first {
                f.write_str(",")?;
            }
            f.write_str(l.as_str())?;
            first = false;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_and_none() {
        for l in Layer::ALL {
            assert!(LayerSet::all().contains(l));
            assert!(!LayerSet::none().contains(l));
        }
        assert_eq!(LayerSet::all().iter().count(), Layer::ALL.len());
    }

    #[test]
    fn names_round_trip() {
        for l in Layer::ALL {
            assert_eq!(l.as_str().parse::<Layer>(), Ok(l));
        }
        let set = LayerSet::from_names(" amenity, shop ,tourism,").expect("valid");
        assert_eq!(set.to_string(), "amenity,shop,tourism");
        assert_eq!(
            LayerSet::from_names(&LayerSet::all().to_string()),
            Ok(LayerSet::all())
        );
    }

    #[test]
    fn unknown_name_lists_available_layers() {
        let err = LayerSet::from_names("amenity,bogus").unwrap_err();
        assert_eq!(err.name, "bogus");
        assert!(err.to_string().contains("education"));
    }
}
