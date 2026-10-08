use serde::{Deserialize, Serialize};

/// RGBA color with f32 components in [0, 1].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Color {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Color {
    pub const fn rgba(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self { r, g, b, a }
    }

    pub const fn rgb(r: f32, g: f32, b: f32) -> Self {
        Self { r, g, b, a: 1.0 }
    }

    /// Parse a hex color string (#RGB, #RGBA, #RRGGBB, #RRGGBBAA).
    pub fn from_hex(hex: &str) -> Option<Self> {
        let hex = hex.strip_prefix('#').unwrap_or(hex);
        // Byte-indexed slicing below is only valid on ASCII input.
        if !hex.is_ascii() {
            return None;
        }
        match hex.len() {
            3 => {
                let r = u8::from_str_radix(&hex[0..1], 16).ok()? * 17;
                let g = u8::from_str_radix(&hex[1..2], 16).ok()? * 17;
                let b = u8::from_str_radix(&hex[2..3], 16).ok()? * 17;
                Some(Self::rgb(
                    r as f32 / 255.0,
                    g as f32 / 255.0,
                    b as f32 / 255.0,
                ))
            }
            4 => {
                let r = u8::from_str_radix(&hex[0..1], 16).ok()? * 17;
                let g = u8::from_str_radix(&hex[1..2], 16).ok()? * 17;
                let b = u8::from_str_radix(&hex[2..3], 16).ok()? * 17;
                let a = u8::from_str_radix(&hex[3..4], 16).ok()? * 17;
                Some(Self::rgba(
                    r as f32 / 255.0,
                    g as f32 / 255.0,
                    b as f32 / 255.0,
                    a as f32 / 255.0,
                ))
            }
            6 => {
                let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
                let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
                let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
                Some(Self::rgb(
                    r as f32 / 255.0,
                    g as f32 / 255.0,
                    b as f32 / 255.0,
                ))
            }
            8 => {
                let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
                let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
                let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
                let a = u8::from_str_radix(&hex[6..8], 16).ok()?;
                Some(Self::rgba(
                    r as f32 / 255.0,
                    g as f32 / 255.0,
                    b as f32 / 255.0,
                    a as f32 / 255.0,
                ))
            }
            _ => None,
        }
    }

    pub const WHITE: Self = Self::rgb(1.0, 1.0, 1.0);
    pub const BLACK: Self = Self::rgb(0.0, 0.0, 0.0);
    pub const TRANSPARENT: Self = Self::rgba(0.0, 0.0, 0.0, 0.0);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx_eq(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-5
    }

    fn color_approx_eq(a: Color, b: Color) -> bool {
        approx_eq(a.r, b.r) && approx_eq(a.g, b.g) && approx_eq(a.b, b.b) && approx_eq(a.a, b.a)
    }

    // --- #RGB shorthand ---

    #[test]
    fn from_hex_rgb_shorthand() {
        let c = Color::from_hex("#f00").expect("#f00 must parse");
        // 'f' → 0xf * 17 = 255 → 1.0; '0' → 0x0 * 17 = 0 → 0.0
        assert!(approx_eq(c.r, 1.0), "r={}", c.r);
        assert!(approx_eq(c.g, 0.0), "g={}", c.g);
        assert!(approx_eq(c.b, 0.0), "b={}", c.b);
        assert!(approx_eq(c.a, 1.0), "a={}", c.a);
    }

    #[test]
    fn from_hex_rgb_shorthand_equals_rgb_constructor() {
        let from_hex = Color::from_hex("#f00").expect("#f00 must parse");
        let direct = Color::rgb(1.0, 0.0, 0.0);
        assert!(color_approx_eq(from_hex, direct));
    }

    // --- #RGBA shorthand ---

    #[test]
    fn from_hex_rgba_shorthand() {
        // #80f0 → r=0x88/255, g=0x00/255, b=0xff/255, a=0x00/255
        let c = Color::from_hex("#80f0").expect("#80f0 must parse");
        assert!(approx_eq(c.r, (0x8 * 17) as f32 / 255.0), "r={}", c.r);
        assert!(approx_eq(c.g, 0.0), "g={}", c.g);
        assert!(approx_eq(c.b, (0xf * 17) as f32 / 255.0), "b={}", c.b);
        assert!(approx_eq(c.a, 0.0), "a={}", c.a);
    }

    // --- #RRGGBB ---

    #[test]
    fn from_hex_rrggbb() {
        let c = Color::from_hex("#ff0000").expect("#ff0000 must parse");
        assert!(approx_eq(c.r, 1.0));
        assert!(approx_eq(c.g, 0.0));
        assert!(approx_eq(c.b, 0.0));
        assert!(approx_eq(c.a, 1.0));
    }

    #[test]
    fn from_hex_rrggbb_mid_gray() {
        let c = Color::from_hex("#808080").expect("#808080 must parse");
        let expected = 0x80_u8 as f32 / 255.0;
        assert!(approx_eq(c.r, expected));
        assert!(approx_eq(c.g, expected));
        assert!(approx_eq(c.b, expected));
        assert!(approx_eq(c.a, 1.0));
    }

    // --- #RRGGBBAA ---

    #[test]
    fn from_hex_rrggbbaa() {
        let c = Color::from_hex("#ff000080").expect("#ff000080 must parse");
        assert!(approx_eq(c.r, 1.0));
        assert!(approx_eq(c.g, 0.0));
        assert!(approx_eq(c.b, 0.0));
        assert!(approx_eq(c.a, 0x80_u8 as f32 / 255.0));
    }

    // --- Without # prefix ---

    #[test]
    fn from_hex_without_hash_prefix() {
        let with_hash = Color::from_hex("#ff8800").expect("must parse");
        let without_hash = Color::from_hex("ff8800").expect("must parse without #");
        assert!(color_approx_eq(with_hash, without_hash));
    }

    // --- Invalid input returns None ---

    #[test]
    fn from_hex_invalid_length_returns_none() {
        assert!(Color::from_hex("#ff").is_none(), "2-char hex must fail");
        assert!(Color::from_hex("#fffff").is_none(), "5-char hex must fail");
        assert!(
            Color::from_hex("#fffffff").is_none(),
            "7-char hex must fail"
        );
        assert!(Color::from_hex("").is_none(), "empty string must fail");
    }

    #[test]
    fn from_hex_invalid_chars_returns_none() {
        assert!(Color::from_hex("#zzzzzz").is_none());
        assert!(Color::from_hex("#gg0000").is_none());
    }

    #[test]
    fn from_hex_non_ascii_returns_none_instead_of_panicking() {
        assert!(Color::from_hex("é1").is_none());
        assert!(Color::from_hex("#ffé").is_none());
    }

    // --- Shorthand vs full-form equivalence ---

    #[test]
    fn shorthand_f00_equals_rgb_full() {
        // #f00 is #ff0000 (each nibble repeated)
        let short = Color::from_hex("#f00").expect("#f00");
        let full = Color::from_hex("#ff0000").expect("#ff0000");
        assert!(color_approx_eq(short, full));
    }
}
