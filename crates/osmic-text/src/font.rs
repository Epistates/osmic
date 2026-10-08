//! Font stacks: MapLibre `text-font` names resolved to installed faces.

use std::sync::Arc;

use cosmic_text::{Style, Weight, fontdb};

/// An ordered list of font names, as in MapLibre's `text-font`
/// (`["Open Sans Bold", "Arial Unicode MS Bold"]`). The first name that
/// matches a loaded font is used; if none does, the engine's default
/// sans-serif family is. Glyphs missing from the chosen font still fall
/// back to other fonts.
///
/// A name is a family optionally followed by a weight and style, the way
/// MapLibre glyph servers name font stacks: `Noto Sans Semi Bold Italic`
/// is the family `Noto Sans` at weight 600, italic.
///
/// Cloning is cheap (the names are shared).
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct FontStack(Arc<[String]>);

impl FontStack {
    /// A stack of `names`, in priority order.
    pub fn new<S: Into<String>>(names: impl IntoIterator<Item = S>) -> Self {
        Self(names.into_iter().map(Into::into).collect())
    }

    /// The names, in priority order.
    pub fn names(&self) -> &[String] {
        &self.0
    }
}

impl From<Arc<[String]>> for FontStack {
    fn from(names: Arc<[String]>) -> Self {
        Self(names)
    }
}

/// A font name resolved against the font database.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ResolvedFont {
    /// The family as the database spells it.
    pub family: String,
    pub weight: Weight,
    pub style: Style,
}

/// The first name of `stack` that names a family in `db`.
pub(crate) fn resolve(db: &fontdb::Database, stack: &FontStack) -> Option<ResolvedFont> {
    stack.names().iter().find_map(|name| resolve_name(db, name))
}

/// Split `name` into a family the database has and a weight/style suffix.
/// Longer family prefixes are tried first, so a family whose own name
/// ends in a style word (`Font Light`, if installed as such) still wins.
fn resolve_name(db: &fontdb::Database, name: &str) -> Option<ResolvedFont> {
    let words: Vec<&str> = name.split_whitespace().collect();
    (1..=words.len()).rev().find_map(|cut| {
        let (weight, style) = parse_suffix(&words[cut..])?;
        let family = words[..cut].join(" ");
        let found = db.faces().find_map(|face| {
            face.families
                .iter()
                .find(|(f, _)| f.eq_ignore_ascii_case(&family))
                .map(|(f, _)| f.clone())
        })?;
        Some(ResolvedFont {
            family: found,
            weight,
            style,
        })
    })
}

/// Weight and style from trailing words such as `Semi Bold Italic`;
/// `None` if any word is not a weight or style.
fn parse_suffix(words: &[&str]) -> Option<(Weight, Style)> {
    let mut text: String = words.iter().map(|w| w.to_ascii_lowercase()).collect();
    let mut style = Style::Normal;
    for (suffix, s) in [("italic", Style::Italic), ("oblique", Style::Oblique)] {
        if let Some(rest) = text.strip_suffix(suffix) {
            style = s;
            text = rest.to_string();
            break;
        }
    }
    let weight = match text.as_str() {
        "" | "regular" | "normal" | "book" | "roman" => 400,
        "thin" | "hairline" => 100,
        "extralight" | "ultralight" => 200,
        "light" => 300,
        "medium" => 500,
        "semibold" | "demibold" => 600,
        "bold" => 700,
        "extrabold" | "ultrabold" => 800,
        "black" | "heavy" => 900,
        _ => return None,
    };
    Some((Weight(weight), style))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> fontdb::Database {
        let mut db = fontdb::Database::new();
        db.load_font_data(include_bytes!("../tests/fonts/Cantarell-Regular.ttf").to_vec());
        db
    }

    #[test]
    fn names_split_into_family_weight_and_style() {
        let db = db();
        let r = |name: &str| resolve(&db, &FontStack::new([name]));
        let regular = r("Cantarell Regular").unwrap();
        assert_eq!(
            (regular.family.as_str(), regular.weight, regular.style),
            ("Cantarell", Weight::NORMAL, Style::Normal)
        );
        let bold = r("cantarell semi bold italic").unwrap();
        assert_eq!(
            (bold.family.as_str(), bold.weight, bold.style),
            ("Cantarell", Weight(600), Style::Italic)
        );
        assert_eq!(r("Cantarell").unwrap().weight, Weight::NORMAL);
        assert_eq!(r("Cantarell Black").unwrap().weight, Weight::BLACK);
        assert!(r("Open Sans Regular").is_none(), "not loaded");
        assert!(r("Cantarell Wide").is_none(), "unknown suffix");
        assert!(r("").is_none());
    }

    #[test]
    fn the_first_loaded_name_in_the_stack_wins() {
        let db = db();
        let stack = FontStack::new(["Open Sans Bold", "Cantarell Bold", "Cantarell Light"]);
        let r = resolve(&db, &stack).unwrap();
        assert_eq!((r.family.as_str(), r.weight), ("Cantarell", Weight::BOLD));
        assert!(resolve(&db, &FontStack::default()).is_none());
    }
}
