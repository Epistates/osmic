//! Tag filters: a small, composable predicate over OSM tags plus a text
//! syntax for command lines.
//!
//! # Syntax
//!
//! A filter is a whitespace-separated list of terms; an element matches if
//! **any** term matches.
//!
//! | Term            | Matches when                                   |
//! |-----------------|------------------------------------------------|
//! | `key=value`     | the tag `key` has exactly `value`              |
//! | `key=*` / `key` | the element has the key (any value)            |
//! | `key!=value`    | the key is absent or has a different value     |
//! | `!key`          | the key is absent                              |
//!
//! Keys and values may be double-quoted to include spaces, `=`, `!` or
//! quotes (`\"` escapes a quote): `name="Joe's Pizza"`, `"addr:street"=*`.
//!
//! AND/NOT composition is available programmatically through
//! [`TagFilter::all`] and [`TagFilter::negate`].
//!
//! ```
//! use osmic_osm::filter::TagFilter;
//!
//! let f = TagFilter::parse(r#"shop=* amenity=restaurant name="Joe's Pizza""#).unwrap();
//! assert!(f.matches(&[("shop", "bakery")]));
//! assert!(f.matches(&[("name", "Joe's Pizza")]));
//! assert!(!f.matches(&[("amenity", "bank")]));
//! ```

use serde::{Deserialize, Serialize};

/// A composable tag predicate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum TagFilter {
    /// `key=value`.
    Tag {
        /// Tag key, compared exactly.
        key: String,
        /// Tag value, compared exactly.
        value: String,
    },
    /// The key is present (any value).
    KeyExists {
        /// Tag key, compared exactly.
        key: String,
    },
    /// Every sub-filter matches (an empty list matches everything).
    All(Vec<TagFilter>),
    /// At least one sub-filter matches (an empty list matches nothing).
    Any(Vec<TagFilter>),
    /// The sub-filter does not match.
    Not(Box<TagFilter>),
}

/// A filter string that could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid tag filter at character {position}: {message}")]
pub struct FilterParseError {
    /// Byte offset into the input where parsing failed (the input length
    /// if it ended early).
    pub position: usize,
    /// What was expected or wrong there.
    pub message: String,
}

impl TagFilter {
    /// Matches when `key` has exactly `value`.
    pub fn tag(key: &str, value: &str) -> Self {
        Self::Tag {
            key: key.to_string(),
            value: value.to_string(),
        }
    }

    /// Matches when `key` is present, whatever its value.
    pub fn key_exists(key: &str) -> Self {
        Self::KeyExists {
            key: key.to_string(),
        }
    }

    /// Matches when every filter matches (an empty list matches everything).
    pub fn all(filters: Vec<TagFilter>) -> Self {
        Self::All(filters)
    }

    /// Matches when any filter matches (an empty list matches nothing).
    pub fn any(filters: Vec<TagFilter>) -> Self {
        Self::Any(filters)
    }

    /// Matches when `filter` does not.
    #[allow(clippy::should_implement_trait)]
    pub fn negate(filter: TagFilter) -> Self {
        Self::Not(Box::new(filter))
    }

    /// A filter matching every element.
    pub fn everything() -> Self {
        Self::All(Vec::new())
    }

    /// Whether `tags` satisfy the filter.
    pub fn matches<K: AsRef<str>, V: AsRef<str>>(&self, tags: &[(K, V)]) -> bool {
        match self {
            Self::Tag { key, value } => tags
                .iter()
                .any(|(k, v)| k.as_ref() == key && v.as_ref() == value),
            Self::KeyExists { key } => tags.iter().any(|(k, _)| k.as_ref() == key),
            Self::All(fs) => fs.iter().all(|f| f.matches(tags)),
            Self::Any(fs) => fs.iter().any(|f| f.matches(tags)),
            Self::Not(f) => !f.matches(tags),
        }
    }

    /// Parse the text syntax described in the module docs.
    ///
    /// # Errors
    ///
    /// [`FilterParseError`] for empty input, an unterminated or misplaced
    /// quote, a missing key or value, or a malformed operator.
    pub fn parse(input: &str) -> Result<Self, FilterParseError> {
        let mut p = Parser {
            chars: input.char_indices().collect(),
            pos: 0,
        };
        let mut terms = Vec::new();
        loop {
            p.skip_ws();
            if p.done() {
                break;
            }
            terms.push(p.term()?);
        }
        match terms.len() {
            0 => Err(FilterParseError {
                position: 0,
                message: "empty filter".into(),
            }),
            1 => Ok(terms.remove(0)),
            _ => Ok(Self::Any(terms)),
        }
    }
}

impl std::str::FromStr for TagFilter {
    type Err = FilterParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

struct Parser {
    chars: Vec<(usize, char)>,
    pos: usize,
}

impl Parser {
    fn done(&self) -> bool {
        self.pos >= self.chars.len()
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).map(|&(_, c)| c)
    }

    fn offset(&self) -> usize {
        self.chars.get(self.pos).map_or_else(
            || self.chars.last().map_or(0, |&(i, c)| i + c.len_utf8()),
            |&(i, _)| i,
        )
    }

    fn err(&self, message: &str) -> FilterParseError {
        FilterParseError {
            position: self.offset(),
            message: message.to_string(),
        }
    }

    fn skip_ws(&mut self) {
        while self.peek().is_some_and(char::is_whitespace) {
            self.pos += 1;
        }
    }

    /// A bare or quoted word, ending at whitespace or one of `stop`.
    fn word(&mut self, stop: &[char]) -> Result<String, FilterParseError> {
        if self.peek() == Some('"') {
            self.pos += 1;
            let mut s = String::new();
            loop {
                match self.peek() {
                    None => return Err(self.err("unterminated quote")),
                    Some('"') => {
                        self.pos += 1;
                        return Ok(s);
                    }
                    Some('\\') if self.chars.get(self.pos + 1).map(|c| c.1) == Some('"') => {
                        s.push('"');
                        self.pos += 2;
                    }
                    Some(c) => {
                        s.push(c);
                        self.pos += 1;
                    }
                }
            }
        }
        let mut s = String::new();
        while let Some(c) = self.peek() {
            if c.is_whitespace() || stop.contains(&c) {
                break;
            }
            if c == '"' {
                return Err(self.err("quote inside an unquoted word"));
            }
            s.push(c);
            self.pos += 1;
        }
        Ok(s)
    }

    fn term(&mut self) -> Result<TagFilter, FilterParseError> {
        let negated = self.peek() == Some('!');
        if negated {
            self.pos += 1;
        }
        let key = self.word(&['=', '!'])?;
        if key.is_empty() {
            return Err(self.err("expected a key"));
        }
        if negated {
            if matches!(self.peek(), Some('=' | '!')) {
                return Err(self.err("`!key` takes no value; use `key!=value`"));
            }
            return Ok(TagFilter::negate(TagFilter::KeyExists { key }));
        }
        let not_equal = match self.peek() {
            Some('!') => {
                self.pos += 1;
                if self.peek() != Some('=') {
                    return Err(self.err("expected `=` after `!`"));
                }
                self.pos += 1;
                true
            }
            Some('=') => {
                self.pos += 1;
                false
            }
            _ => return Ok(TagFilter::KeyExists { key }),
        };
        let quoted = self.peek() == Some('"');
        let value = self.word(&[])?;
        let filter = if value == "*" && !quoted {
            TagFilter::KeyExists { key }
        } else if value.is_empty() && !quoted {
            return Err(self.err("expected a value (use `key=*` for any value)"));
        } else {
            TagFilter::Tag { key, value }
        };
        Ok(if not_equal {
            TagFilter::negate(filter)
        } else {
            filter
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(filter: &str, tags: &[(&str, &str)]) -> bool {
        TagFilter::parse(filter).expect("valid").matches(tags)
    }

    #[test]
    fn terms_are_ored() {
        let f = "office=property_management office=estate_agent";
        assert!(m(f, &[("office", "property_management")]));
        assert!(m(f, &[("office", "estate_agent")]));
        assert!(!m(f, &[("office", "company")]));
    }

    #[test]
    fn wildcard_and_bare_key() {
        assert!(m("name=*", &[("name", "Acme")]));
        assert!(m("name", &[("name", "Acme")]));
        assert!(!m("name=*", &[("office", "x")]));
    }

    #[test]
    fn negation() {
        assert!(m("!access", &[("name", "x")]));
        assert!(!m("!access", &[("access", "private")]));
        assert!(m("access!=private", &[("access", "yes")]));
        assert!(m("access!=private", &[]));
        assert!(!m("access!=private", &[("access", "private")]));
    }

    #[test]
    fn quoting() {
        assert!(m(r#"name="Joe's Pizza""#, &[("name", "Joe's Pizza")]));
        assert!(m(r#""addr:street"=*"#, &[("addr:street", "Main")]));
        assert!(m(r#"name="say \"hi\"""#, &[("name", "say \"hi\"")]));
        assert!(m(r#"note="=*""#, &[("note", "=*")]), "quoted * is literal");
        assert!(m(r#"name="""#, &[("name", "")]), "explicit empty value");
    }

    #[test]
    fn errors_are_reported_with_positions() {
        for bad in [
            "",
            "   ",
            "=value",
            "key=",
            r#"name="open"#,
            "!key=x",
            "a!b",
            r#"na"me=x"#,
        ] {
            assert!(TagFilter::parse(bad).is_err(), "{bad:?} should fail");
        }
        assert_eq!(TagFilter::parse("ok key=").unwrap_err().position, 7);
    }

    #[test]
    fn programmatic_composition() {
        let f = TagFilter::all(vec![
            TagFilter::parse("shop=*").expect("valid"),
            TagFilter::negate(TagFilter::tag("brand", "McDonalds")),
        ]);
        assert!(f.matches(&[("shop", "tyres")]));
        assert!(!f.matches(&[("shop", "fast_food"), ("brand", "McDonalds")]));
        assert!(TagFilter::everything().matches::<&str, &str>(&[]));
        assert!(!TagFilter::any(vec![]).matches::<&str, &str>(&[]));
    }

    #[test]
    fn owned_and_borrowed_tags() {
        let f = TagFilter::tag("a", "b");
        assert!(f.matches(&[("a".to_string(), "b".to_string())]));
        assert!(f.matches(&[("a", "b")]));
    }
}
