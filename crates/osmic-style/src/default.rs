//! The default osmic style, defined once as data.
//!
//! Every consumer — MapLibre clients (via [`Style::to_json`]), the software
//! renderer and the interactive viewer — reads this definition, so the
//! palette cannot drift between them. It targets the osmic tile schema:
//! vector-tile layers are named after [`osmic_osm::Layer`] and features are
//! classified by their `class` attribute (the raw OSM value) with an
//! optional `name`.

use osmic_core::Color;
use osmic_osm::Layer as TileLayer;

use crate::expr::{Expr, Interpolation};
use crate::model::{
    BackgroundLayer, CircleLayer, FillLayer, Layer, LayerKind, LineLayer, Style, SymbolLayer,
    VectorSource,
};
use crate::property::{LineCap, LineJoin, Property, SymbolPlacement, TextAnchor};
use crate::value::Value;

/// Id of the vector source every default layer reads.
pub const DEFAULT_SOURCE_ID: &str = "osmic";

/// Attribution required by the OpenStreetMap data licence.
pub const ATTRIBUTION: &str = "© OpenStreetMap contributors";

/// Public glyph server used when none is configured. Clients fetch label
/// glyphs from it, so deployments that must not depend on a third party
/// should set [`StyleOptions::glyphs`] to their own server (or `None` and
/// provide `glyphs` another way).
pub const DEFAULT_GLYPHS_URL: &str = "https://fonts.openmaptiles.org/{fontstack}/{range}.pbf";

/// Source URL used by [`default_style`], which is meant for renderers that
/// never fetch it.
const PLACEHOLDER_SOURCE_URL: &str = "pmtiles://osmic.pmtiles";

const BACKGROUND: &str = "#f8f4f0";
const WATER: &str = "#aad3df";
const WOOD: &str = "#add19e";
const GRASS: &str = "#cdebb0";
const HALO: &str = "#ffffff";

/// Options for building the default style.
#[derive(Debug, Clone, PartialEq)]
pub struct StyleOptions {
    /// Where tiles come from: a `.pmtiles` path or URL, a `{z}/{x}/{y}`
    /// template, or a TileJSON URL.
    pub source_url: String,
    /// Glyph server template (`{fontstack}` and `{range}`), or `None` to
    /// leave `glyphs` unset.
    pub glyphs: Option<String>,
    /// Font stack for labels. Must exist on the glyph server.
    pub text_font: Vec<String>,
}

impl StyleOptions {
    /// Options for `source_url` with the default glyph server and fonts.
    pub fn new(source_url: impl Into<String>) -> Self {
        Self {
            source_url: source_url.into(),
            glyphs: Some(DEFAULT_GLYPHS_URL.to_string()),
            text_font: vec!["Open Sans Regular".to_string()],
        }
    }
}

impl VectorSource {
    /// The osmic source for `url`.
    ///
    /// * `*.pmtiles` paths and URLs (query and fragment ignored) get the
    ///   `pmtiles://` scheme if they lack it;
    /// * URLs containing `{z}` become a tile template;
    /// * anything else is treated as a TileJSON URL.
    pub fn from_url(url: &str) -> Self {
        let path = url.split(['?', '#']).next().unwrap_or(url);
        let mut source = Self {
            attribution: Some(ATTRIBUTION.to_string()),
            ..Self::default()
        };
        if url.starts_with("pmtiles://") {
            source.url = Some(url.to_string());
        } else if path.ends_with(".pmtiles") {
            source.url = Some(format!("pmtiles://{url}"));
        } else if url.contains("{z}") {
            source.tiles = vec![url.to_string()];
        } else {
            source.url = Some(url.to_string());
        }
        source
    }
}

/// The default style for tiles at `source_url`, serialisable with
/// [`Style::to_json`].
pub fn default_style_json(source_url: &str) -> Style {
    default_style_with(&StyleOptions::new(source_url))
}

/// The default style with a placeholder source, for renderers that are
/// handed tiles directly.
pub fn default_style() -> Style {
    default_style_json(PLACEHOLDER_SOURCE_URL)
}

/// The default style built from `options`.
pub fn default_style_with(options: &StyleOptions) -> Style {
    let mut style = Style::new("Osmic Default");
    style.glyphs = options.glyphs.clone();
    style.set_vector_source(
        DEFAULT_SOURCE_ID,
        &VectorSource::from_url(&options.source_url),
    );
    style.layers = layers(&options.text_font);
    style
}

// --- builders ---------------------------------------------------------

fn hex(s: &str) -> Color {
    Color::parse(s).expect("palette colors are valid CSS colors")
}

fn color(s: &str) -> Option<Property<Color>> {
    Some(Property::Constant(hex(s)))
}

fn num(n: f64) -> Option<Property<f64>> {
    Some(Property::Constant(n))
}

fn lit_color(s: &str) -> Expr {
    Expr::Literal(Value::Color(hex(s)))
}

/// `class` → color table with a fallback.
fn class_color(arms: &[(&[&str], &str)], fallback: &str) -> Option<Property<Color>> {
    Some(Property::Expr(Expr::match_get(
        "class",
        arms.iter()
            .map(|(classes, c)| (classes.to_vec(), lit_color(c)))
            .collect(),
        lit_color(fallback),
    )))
}

fn class_number(arms: &[(&[&str], f64)], fallback: f64) -> Expr {
    Expr::match_get(
        "class",
        arms.iter()
            .map(|(classes, n)| (classes.to_vec(), Expr::number(*n)))
            .collect(),
        Expr::number(fallback),
    )
}

fn class_in_number(arms: &[(&[&str], f64)], fallback: f64) -> Option<Property<f64>> {
    Some(Property::Expr(class_number(arms, fallback)))
}

/// A per-class width that also scales with zoom: `scales` are
/// `(zoom, multiplier)` stops applied to the class table.
fn zoom_scaled(
    arms: &[(&[&str], f64)],
    fallback: f64,
    scales: &[(f64, f64)],
) -> Option<Property<f64>> {
    let stops = scales
        .iter()
        .map(|(zoom, scale)| {
            let scaled: Vec<(&[&str], f64)> = arms.iter().map(|(c, w)| (*c, w * scale)).collect();
            (*zoom, class_number(&scaled, fallback * scale))
        })
        .collect();
    Some(Property::Expr(Expr::interpolate_zoom(
        Interpolation::Exponential(1.4),
        stops,
    )))
}

fn has_name() -> Expr {
    Expr::has("name")
}

fn classes(values: &[&str]) -> Expr {
    Expr::get_in("class", values)
}

fn fill(
    id: &str,
    tile: TileLayer,
    min_zoom: f64,
    color: Option<Property<Color>>,
    opacity: f64,
) -> Layer {
    Layer::new(
        id,
        LayerKind::Fill(FillLayer {
            color,
            opacity: num(opacity),
        }),
    )
    .from_source(DEFAULT_SOURCE_ID, tile.as_str())
    .zoom_range(Some(min_zoom), None)
}

fn line(id: &str, tile: TileLayer, min_zoom: f64, line: LineLayer) -> Layer {
    Layer::new(id, LayerKind::Line(line))
        .from_source(DEFAULT_SOURCE_ID, tile.as_str())
        .zoom_range(Some(min_zoom), None)
}

fn round_line(color: Option<Property<Color>>, width: Option<Property<f64>>) -> LineLayer {
    LineLayer {
        cap: Some(Property::Constant(LineCap::Round)),
        join: Some(Property::Constant(LineJoin::Round)),
        color,
        width,
        ..LineLayer::default()
    }
}

struct Label<'a> {
    id: &'a str,
    tile: TileLayer,
    min_zoom: f64,
    filter: Expr,
    size: Option<Property<f64>>,
    color: &'a str,
    halo_width: f64,
    padding: f64,
    placement: SymbolPlacement,
    sort_key: Option<Property<f64>>,
    /// Text sits below its anchor point (for labels that accompany a dot).
    below_point: bool,
}

impl<'a> Label<'a> {
    fn new(id: &'a str, tile: TileLayer, min_zoom: f64, color: &'a str) -> Self {
        Self {
            id,
            tile,
            min_zoom,
            filter: has_name(),
            size: num(10.0),
            color,
            halo_width: 1.0,
            padding: 5.0,
            placement: SymbolPlacement::Point,
            sort_key: None,
            below_point: false,
        }
    }

    fn build(self, font: &[String]) -> Layer {
        let line_placed = self.placement == SymbolPlacement::Line;
        let symbol = SymbolLayer {
            placement: (self.placement != SymbolPlacement::Point)
                .then_some(Property::Constant(self.placement)),
            sort_key: self.sort_key,
            text_field: Some(Property::Expr(Expr::get("name"))),
            text_font: Some(Property::Constant(font.into())),
            text_size: self.size,
            text_anchor: self
                .below_point
                .then_some(Property::Constant(TextAnchor::Top)),
            text_offset: self
                .below_point
                .then_some(Property::Constant(vec![0.0, 0.6])),
            text_padding: num(self.padding),
            text_max_angle: line_placed.then_some(Property::Constant(30.0)),
            text_rotation_alignment: line_placed
                .then_some(Property::Constant(crate::property::Alignment::Map)),
            text_color: color(self.color),
            text_halo_color: color(HALO),
            text_halo_width: num(self.halo_width),
            ..SymbolLayer::default()
        };
        Layer::new(self.id, LayerKind::Symbol(symbol))
            .from_source(DEFAULT_SOURCE_ID, self.tile.as_str())
            .zoom_range(Some(self.min_zoom), None)
            .with_filter(self.filter)
    }
}

/// One point-of-interest layer: a dot from `dot_min_zoom` and a label from
/// `label_min_zoom`.
struct Poi {
    tile: TileLayer,
    label_min_zoom: f64,
    text_color: &'static str,
    dot: Option<Property<Color>>,
    dot_radius: f64,
}

fn poi_layers(font: &[String]) -> (Vec<Layer>, Vec<Layer>) {
    let pois = [
        Poi {
            tile: TileLayer::Amenity,
            label_min_zoom: 15.0,
            text_color: "#734a08",
            dot: class_color(
                &[
                    (
                        &["restaurant", "cafe", "bar", "pub", "fast_food"],
                        "#d96c22",
                    ),
                    (&["hospital", "clinic", "pharmacy", "doctors"], "#c8372d"),
                    (
                        &["school", "university", "college", "kindergarten"],
                        "#f0c330",
                    ),
                    (&["bank", "atm"], "#445566"),
                    (&["fuel", "charging_station", "car_wash"], "#2878a6"),
                ],
                "#6f6f6f",
            ),
            dot_radius: 3.0,
        },
        Poi {
            tile: TileLayer::Shop,
            label_min_zoom: 15.0,
            text_color: "#5b3a0a",
            dot: color("#ac39ac"),
            dot_radius: 3.5,
        },
        Poi {
            tile: TileLayer::Tourism,
            label_min_zoom: 14.0,
            text_color: "#0d7377",
            dot: color("#3fa34d"),
            dot_radius: 3.5,
        },
        Poi {
            tile: TileLayer::Healthcare,
            label_min_zoom: 15.0,
            text_color: "#c4281c",
            dot: color("#c8372d"),
            dot_radius: 3.5,
        },
        Poi {
            tile: TileLayer::Office,
            label_min_zoom: 15.0,
            text_color: "#555555",
            dot: color("#4a6fa5"),
            dot_radius: 3.0,
        },
        Poi {
            tile: TileLayer::Craft,
            label_min_zoom: 15.0,
            text_color: "#b5651d",
            dot: color("#8b5a3c"),
            dot_radius: 3.0,
        },
        Poi {
            tile: TileLayer::Historic,
            label_min_zoom: 14.0,
            text_color: "#7b2d8b",
            dot: color("#7a5c40"),
            dot_radius: 3.0,
        },
        Poi {
            tile: TileLayer::Club,
            label_min_zoom: 15.0,
            text_color: "#555588",
            dot: color("#6a6a9a"),
            dot_radius: 3.0,
        },
        Poi {
            tile: TileLayer::Emergency,
            label_min_zoom: 15.0,
            text_color: "#cc0000",
            dot: color("#cc0000"),
            dot_radius: 3.0,
        },
        Poi {
            tile: TileLayer::Education,
            label_min_zoom: 15.0,
            text_color: "#336699",
            dot: color("#336699"),
            dot_radius: 3.0,
        },
    ];
    let mut dots = Vec::new();
    let mut labels = Vec::new();
    for poi in pois {
        let name = poi.tile.as_str();
        dots.push(
            Layer::new(
                format!("{name}-dot"),
                LayerKind::Circle(CircleLayer {
                    radius: num(poi.dot_radius),
                    color: poi.dot,
                    stroke_color: color(HALO),
                    stroke_width: num(0.75),
                    ..CircleLayer::default()
                }),
            )
            .from_source(DEFAULT_SOURCE_ID, name)
            .zoom_range(Some(poi.label_min_zoom - 1.0), None),
        );
        let id = format!("{name}-label");
        let mut label = Label::new(&id, poi.tile, poi.label_min_zoom, poi.text_color);
        label.below_point = true;
        labels.push(label.build(font));
    }
    (dots, labels)
}

fn layers(font: &[String]) -> Vec<Layer> {
    let mut layers = vec![Layer::new(
        "background",
        LayerKind::Background(BackgroundLayer {
            color: color(BACKGROUND),
            opacity: None,
        }),
    )];

    layers.push(fill(
        "landuse-fill",
        TileLayer::Landuse,
        7.0,
        class_color(
            &[
                (&["forest"], WOOD),
                (&["grass", "meadow"], GRASS),
                (&["farmland"], "#d5e29e"),
                (&["residential"], "#e0d6d0"),
                (&["commercial"], "#f2dad9"),
                (&["industrial"], "#ebdbe8"),
                (&["cemetery"], "#aacbaf"),
            ],
            "#d5cfc8",
        ),
        0.8,
    ));
    layers.push(fill(
        "natural-fill",
        TileLayer::Natural,
        6.0,
        class_color(
            &[
                (&["wood"], WOOD),
                (&["scrub"], "#c8d7ab"),
                (&["grassland"], GRASS),
                (&["sand"], "#f5e9c6"),
                (&["beach"], "#fff1ba"),
                (&["glacier"], "#ddecec"),
                (&["water"], WATER),
            ],
            "#e8e0d8",
        ),
        0.6,
    ));
    layers.push(fill(
        "leisure-fill",
        TileLayer::Leisure,
        8.0,
        class_color(
            &[
                (&["park"], "#c8facc"),
                (&["garden", "nature_reserve"], GRASS),
                (&["golf_course"], "#b5e3b5"),
            ],
            "#c8facc",
        ),
        0.6,
    ));
    layers
        .push(fill("water-fill", TileLayer::Water, 0.0, color(WATER), 0.8).zoom_range(None, None));
    layers.push(
        line(
            "water-line",
            TileLayer::Water,
            0.0,
            round_line(
                color(WATER),
                zoom_scaled(
                    &[(&["river"], 3.0), (&["canal"], 2.0)],
                    1.0,
                    &[(8.0, 0.6), (14.0, 1.0), (18.0, 2.5)],
                ),
            ),
        )
        .zoom_range(None, None)
        .with_filter(classes(&["river", "stream", "canal", "drain", "ditch"])),
    );
    layers.push(fill(
        "building-fill",
        TileLayer::Building,
        13.0,
        color("#dfdbd7"),
        0.8,
    ));
    layers.push(line(
        "building-outline",
        TileLayer::Building,
        14.0,
        LineLayer {
            color: color("#c9c0b8"),
            width: num(0.5),
            ..LineLayer::default()
        },
    ));
    layers.push(line(
        "boundary",
        TileLayer::Boundary,
        2.0,
        LineLayer {
            color: color("#9e9cab"),
            width: num(1.5),
            dasharray: Some(Property::Constant(vec![4.0, 2.0])),
            ..LineLayer::default()
        },
    ));
    layers.push(line(
        "railway",
        TileLayer::Railway,
        8.0,
        LineLayer {
            color: color("#bfbfbf"),
            width: class_in_number(&[(&["rail"], 1.5)], 1.0),
            ..LineLayer::default()
        },
    ));

    let road_scale = [(5.0, 0.3), (12.0, 1.0), (18.0, 3.5)];
    layers.push(line(
        "highway-casing",
        TileLayer::Highway,
        7.0,
        round_line(
            color("#c0b8b0"),
            zoom_scaled(
                &[
                    (&["motorway", "motorway_link"], 8.0),
                    (&["trunk", "trunk_link"], 7.0),
                    (&["primary", "primary_link"], 6.0),
                    (&["secondary", "secondary_link"], 5.0),
                    (&["tertiary", "tertiary_link"], 4.0),
                    (&["residential", "unclassified", "living_street"], 3.0),
                    (&["service"], 2.0),
                ],
                1.5,
                &road_scale,
            ),
        ),
    ));
    layers.push(line(
        "highway-fill",
        TileLayer::Highway,
        4.0,
        round_line(
            class_color(
                &[
                    (&["motorway", "motorway_link"], "#e892a2"),
                    (&["trunk", "trunk_link"], "#f9b29c"),
                    (&["primary", "primary_link"], "#fcd6a4"),
                    (&["secondary", "secondary_link"], "#f7fabf"),
                    (
                        &[
                            "tertiary",
                            "tertiary_link",
                            "residential",
                            "unclassified",
                            "living_street",
                            "service",
                        ],
                        "#ffffff",
                    ),
                ],
                "#cccccc",
            ),
            zoom_scaled(
                &[
                    (&["motorway", "motorway_link"], 6.0),
                    (&["trunk", "trunk_link"], 5.0),
                    (&["primary", "primary_link"], 4.0),
                    (&["secondary", "secondary_link"], 3.0),
                    (&["tertiary", "tertiary_link"], 2.5),
                    (&["residential", "unclassified", "living_street"], 1.5),
                    (&["service"], 1.0),
                ],
                0.75,
                &road_scale,
            ),
        ),
    ));

    let (dots, poi_labels) = poi_layers(font);
    layers.push(
        Layer::new(
            "place-dot",
            LayerKind::Circle(CircleLayer {
                radius: num(2.5),
                color: color("#666666"),
                stroke_color: color(HALO),
                stroke_width: num(0.75),
                ..CircleLayer::default()
            }),
        )
        .from_source(DEFAULT_SOURCE_ID, TileLayer::Place.as_str())
        .zoom_range(Some(6.0), Some(14.0))
        .with_filter(classes(&["town", "village", "hamlet"])),
    );
    layers.extend(dots);

    // Labels last: later layers win label collisions, so places beat
    // roads beat points of interest.
    layers.extend(poi_labels);
    let mut area = Label::new("area-label", TileLayer::Leisure, 12.0, "#3a7a3a");
    area.size = num(11.0);
    area.padding = 10.0;
    layers.push(area.build(font));

    let mut water_line = Label::new("water-label-line", TileLayer::Water, 10.0, "#6b9daf");
    water_line.filter = Expr::All(vec![has_name(), classes(&["river", "stream", "canal"])]);
    water_line.size = num(12.0);
    water_line.padding = 30.0;
    water_line.placement = SymbolPlacement::Line;
    layers.push(water_line.build(font));

    let mut water_area = Label::new("water-label-area", TileLayer::Water, 10.0, "#6b9daf");
    water_area.filter = Expr::All(vec![
        has_name(),
        Expr::Not(Box::new(classes(&[
            "river", "stream", "canal", "drain", "ditch",
        ]))),
    ]);
    water_area.size = num(12.0);
    water_area.padding = 10.0;
    layers.push(water_area.build(font));

    let mut minor = Label::new("highway-label-minor", TileLayer::Highway, 14.0, "#666666");
    minor.filter = Expr::All(vec![
        has_name(),
        classes(&[
            "tertiary",
            "residential",
            "unclassified",
            "service",
            "living_street",
        ]),
    ]);
    minor.padding = 10.0;
    minor.placement = SymbolPlacement::Line;
    layers.push(minor.build(font));

    let mut major = Label::new("highway-label-major", TileLayer::Highway, 10.0, "#555555");
    major.filter = Expr::All(vec![
        has_name(),
        classes(&["motorway", "trunk", "primary", "secondary"]),
    ]);
    major.size = class_in_number(
        &[
            (&["motorway"], 13.0),
            (&["trunk"], 12.0),
            (&["primary"], 11.0),
        ],
        10.0,
    );
    major.halo_width = 1.5;
    major.padding = 20.0;
    major.placement = SymbolPlacement::Line;
    layers.push(major.build(font));

    let mut place = Label::new("place-label", TileLayer::Place, 4.0, "#333333");
    place.filter = has_name();
    place.size = class_in_number(
        &[(&["city"], 20.0), (&["town"], 15.0), (&["village"], 12.0)],
        10.0,
    );
    place.sort_key = class_in_number(
        &[(&["city"], 0.0), (&["town"], 1.0), (&["village"], 2.0)],
        3.0,
    );
    place.halo_width = 2.0;
    layers.push(place.build(font));

    layers
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::value::EvalContext;

    #[test]
    fn pmtiles_url_forms() {
        for (input, expected) in [
            ("pmtiles://tiles.pmtiles", "pmtiles://tiles.pmtiles"),
            ("tiles.pmtiles", "pmtiles://tiles.pmtiles"),
            ("/data/tiles.pmtiles", "pmtiles:///data/tiles.pmtiles"),
            ("https://host/x.pmtiles", "pmtiles://https://host/x.pmtiles"),
            (
                "https://host/x.pmtiles?sig=abc",
                "pmtiles://https://host/x.pmtiles?sig=abc",
            ),
        ] {
            let s = default_style_json(input);
            let src = s.vector_source(DEFAULT_SOURCE_ID).unwrap();
            assert_eq!(src.url.as_deref(), Some(expected), "{input}");
            assert!(src.tiles.is_empty());
        }
    }

    #[test]
    fn template_and_tilejson_sources() {
        let url = "http://localhost:3000/{z}/{x}/{y}.mvt";
        let s = default_style_json(url);
        let src = s.vector_source(DEFAULT_SOURCE_ID).unwrap();
        assert_eq!(src.tiles, vec![url.to_string()]);
        assert!(src.url.is_none());
        let v = s.to_value();
        assert_eq!(v["sources"]["osmic"]["tiles"][0], url);
        assert!(v["sources"]["osmic"].get("url").is_none());

        let s = default_style_json("http://localhost:3000/tiles.json");
        let src = s.vector_source(DEFAULT_SOURCE_ID).unwrap();
        assert_eq!(src.url.as_deref(), Some("http://localhost:3000/tiles.json"));
    }

    #[test]
    fn source_carries_attribution() {
        let v = default_style_json("pmtiles://x.pmtiles").to_value();
        assert_eq!(v["sources"]["osmic"]["attribution"], ATTRIBUTION);
        assert_eq!(v["sources"]["osmic"]["type"], "vector");
    }

    #[test]
    fn glyph_server_is_configurable() {
        assert_eq!(
            default_style_json("pmtiles://x.pmtiles").glyphs.as_deref(),
            Some(DEFAULT_GLYPHS_URL)
        );
        let mut options = StyleOptions::new("pmtiles://x.pmtiles");
        options.glyphs = Some("https://fonts.example/{fontstack}/{range}.pbf".into());
        options.text_font = vec!["Noto Sans Regular".into()];
        let style = default_style_with(&options);
        assert_eq!(
            style.glyphs.as_deref(),
            Some("https://fonts.example/{fontstack}/{range}.pbf")
        );
        let v = style.to_value();
        let label = v["layers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|l| l["id"] == "place-label")
            .unwrap();
        assert_eq!(label["layout"]["text-font"], json!(["Noto Sans Regular"]));

        options.glyphs = None;
        assert!(
            default_style_with(&options)
                .to_value()
                .get("glyphs")
                .is_none()
        );
    }

    #[test]
    fn default_style_is_well_formed() {
        let style = default_style_json("pmtiles://x.pmtiles");
        assert_eq!(style.version, 8);
        assert_eq!(style.name, "Osmic Default");
        assert_eq!(style.layers[0].id, "background");
        let mut ids = std::collections::HashSet::new();
        for layer in &style.layers {
            assert!(ids.insert(layer.id.clone()), "duplicate id {}", layer.id);
            if let Some(sl) = &layer.source_layer {
                assert!(
                    sl.parse::<TileLayer>().is_ok(),
                    "{}: `{sl}` is not an osmic tile layer",
                    layer.id
                );
            }
        }
    }

    #[test]
    fn round_trips_through_json_to_an_identical_model() {
        let style = default_style_json("pmtiles://x.pmtiles");
        let text = style.to_json();
        let parsed = Style::from_json(&text).expect("default style must parse");
        assert_eq!(parsed.layers.len(), style.layers.len());
        for (a, b) in parsed.layers.iter().zip(&style.layers) {
            assert_eq!(a, b, "layer {} changed in the round trip", b.id);
        }
        assert_eq!(parsed, style);
        // And again: serialisation is a fixed point.
        assert_eq!(parsed.to_json(), text);
    }

    #[test]
    fn evaluates_palette_by_class_and_zoom() {
        let style = default_style();
        let fill = |id: &str, class: &str| {
            let Some(LayerKind::Fill(f)) = style.layer(id).map(|l| l.kind.clone()) else {
                panic!("{id} is not a fill layer")
            };
            let props = [("class", class)];
            f.resolve(&EvalContext::new(12.0, &props)).color.to_css()
        };
        assert_eq!(fill("landuse-fill", "forest"), "rgba(173,209,158,0.8)");
        assert_eq!(
            fill("landuse-fill", "something-else"),
            "rgba(213,207,200,0.8)"
        );

        let Some(LayerKind::Line(l)) = style.layer("highway-fill").map(|l| l.kind.clone()) else {
            panic!("highway-fill is not a line layer")
        };
        let props = [("class", "motorway")];
        let at = |z: f64| l.resolve(&EvalContext::new(z, &props));
        assert_eq!(at(12.0).width, 6.0);
        assert!(at(8.0).width < at(12.0).width && at(12.0).width < at(16.0).width);
        assert_eq!(at(12.0).color.to_css(), "#e892a2");
    }

    #[test]
    fn filters_select_by_class() {
        let style = default_style();
        let label = style.layer("highway-label-major").unwrap();
        let yes = [("class", "primary"), ("name", "Main")];
        let no_name = [("class", "primary")];
        let wrong = [("class", "service"), ("name", "Alley")];
        assert!(label.accepts(&EvalContext::new(12.0, &yes)));
        assert!(!label.accepts(&EvalContext::new(12.0, &no_name)));
        assert!(!label.accepts(&EvalContext::new(12.0, &wrong)));
    }
}
