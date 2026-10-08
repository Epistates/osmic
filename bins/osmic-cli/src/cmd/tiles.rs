//! `osmic generate-tiles`

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, bail};
use clap::{Args, ValueEnum};

use osmic_osm::{
    IncompleteWays, LayerSet, NodeStorage, PbfProcessor, PipelineConfig, TagFilter, TagRetention,
    TagStore,
};
use osmic_tiles::pmtiles::ArchiveInfo;
use osmic_tiles::{
    AttributeMode, MvtEncoder, RenderConfig, TileEncoder, TileGenerator, TileGeneratorConfig,
};

use super::{check_output, fmt_bytes, fmt_count, write_file};
use crate::args::{ZoomRange, parse_filter, parse_node_store};

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Format {
    /// Mapbox Vector Tiles (supported by every MapLibre/Mapbox client)
    Mvt,
    /// MapLibre Tiles (requires the `mlt` build feature)
    Mlt,
}

#[derive(Debug, Args)]
pub struct TilesArgs {
    /// Input .osm.pbf (or .geojson) file
    pub input: PathBuf,

    /// Output .pmtiles archive
    pub output: PathBuf,

    /// Zoom range, e.g. "0-14" or "12"
    #[arg(long, default_value = "0-14")]
    pub zoom: ZoomRange,

    /// Comma-separated layers to include (default: all)
    #[arg(long)]
    pub layers: Option<String>,

    #[arg(long, value_enum, default_value_t = Format::Mvt)]
    pub format: Format,

    /// Keep only elements matching this tag filter, e.g. "shop=* amenity=restaurant"
    #[arg(long, value_parser = parse_filter)]
    pub tags: Option<TagFilter>,

    /// Drop elements matching this tag filter (applied after --tags)
    #[arg(long, value_parser = parse_filter)]
    pub exclude_tags: Option<TagFilter>,

    /// Write every OSM tag as a tile attribute (default: class, name and
    /// address/contact fields)
    #[arg(long)]
    pub all_tags: bool,

    /// Tile coordinate extent
    #[arg(long, default_value_t = 4096)]
    pub extent: u32,

    /// Buffer around each tile, in screen pixels
    #[arg(long, default_value_t = 4.0)]
    pub buffer_px: f64,

    /// Simplification tolerance in screen pixels (below max zoom)
    #[arg(long, default_value_t = 0.1)]
    pub simplify_px: f64,

    /// Compressed tile size budget; least important features are dropped
    /// from larger tiles
    #[arg(long, default_value_t = 500_000)]
    pub max_tile_bytes: usize,

    /// Memory for sorting rendered tile data before it spills to disk (MiB)
    #[arg(long, default_value_t = 4096)]
    pub memory_mb: usize,

    /// Directory for temporary sort files (default: system temp dir)
    #[arg(long)]
    pub tmp_dir: Option<PathBuf>,

    /// Node location storage: sparse, dense[:MAX_ID] or file:PATH[:MAX_ID]
    /// (a file store can be reused by `osmic update`)
    #[arg(long, default_value = "sparse", value_parser = parse_node_store)]
    pub node_store: NodeStorage,

    /// Build ways from their available nodes when some are missing
    /// (default: skip them)
    #[arg(long)]
    pub keep_incomplete_ways: bool,

    /// Also write a MapLibre style JSON to this path
    #[arg(long)]
    pub style: Option<PathBuf>,

    /// Source URL to embed in the style (default: pmtiles://<OUTPUT>)
    #[arg(long, requires = "style")]
    pub style_url: Option<String>,

    /// Archive name written to the metadata
    #[arg(long, default_value = "osmic")]
    pub name: String,

    /// Overwrite existing output files
    #[arg(long)]
    pub force: bool,
}

fn is_geojson(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("geojson" | "json")
    )
}

pub fn run(args: TilesArgs) -> anyhow::Result<()> {
    let start = Instant::now();
    if !args.input.is_file() {
        bail!("input file {} does not exist", args.input.display());
    }
    check_output(&args.output, args.force)?;
    if let Some(style) = &args.style {
        check_output(style, args.force)?;
    }
    let layers = match &args.layers {
        Some(l) => LayerSet::from_names(l)?,
        None => LayerSet::all(),
    };
    if layers.is_empty() {
        bail!("--layers selects no layers");
    }
    let filter = match (args.tags.clone(), args.exclude_tags.clone()) {
        (None, None) => None,
        (Some(i), None) => Some(i),
        (None, Some(e)) => Some(TagFilter::negate(e)),
        (Some(i), Some(e)) => Some(TagFilter::all(vec![i, TagFilter::negate(e)])),
    };
    let temp_parent = args.tmp_dir.clone().unwrap_or_else(std::env::temp_dir);
    if !temp_parent.is_dir() {
        bail!(
            "temporary directory {} does not exist",
            temp_parent.display()
        );
    }
    crate::cleanup::register_dir(&temp_parent);
    let config = generator_config(&args, temp_parent);

    eprintln!("Input:   {}", args.input.display());
    eprintln!("Output:  {}", args.output.display());
    eprintln!("Zoom:    {}-{}", args.zoom.min, args.zoom.max);
    eprintln!("Layers:  {layers}");

    let (generator, features) = if is_geojson(&args.input) {
        let data = osmic_osm::geojson::load_geojson(&args.input, layers)?;
        let kept: Vec<_> = data
            .features
            .into_iter()
            .filter(|feat| {
                filter.as_ref().is_none_or(|f| {
                    let tags: Vec<(&str, &str)> = data.tag_store.resolve_tags(&feat.tags).collect();
                    f.matches(&tags)
                })
            })
            .collect();
        let generator = TileGenerator::new(
            config,
            make_encoder(args.format)?,
            Arc::clone(&data.tag_store),
        )?;
        generator.add_parallel(&kept)?;
        (generator, kept.len() as u64)
    } else {
        let tag_store = Arc::new(TagStore::new());
        let generator =
            TileGenerator::new(config, make_encoder(args.format)?, Arc::clone(&tag_store))?;
        let processor = PbfProcessor::with_tag_store(
            PipelineConfig {
                layers,
                node_storage: args.node_store.clone(),
                tag_retention: if args.all_tags {
                    TagRetention::All
                } else {
                    TagRetention::Curated
                },
                incomplete_ways: if args.keep_incomplete_ways {
                    IncompleteWays::KeepAvailable
                } else {
                    IncompleteWays::Skip
                },
                filter,
            },
            tag_store,
        );
        let out = processor
            .run(&args.input, &generator)
            .with_context(|| format!("processing {}", args.input.display()))?;
        (generator, out.stats.feature_count)
    };
    finish(generator, &args, features, start)
}

fn make_encoder(format: Format) -> anyhow::Result<Box<dyn TileEncoder>> {
    Ok(match format {
        Format::Mvt => Box::new(MvtEncoder),
        #[cfg(feature = "mlt")]
        Format::Mlt => Box::new(osmic_tiles::MltEncoder),
        #[cfg(not(feature = "mlt"))]
        Format::Mlt => bail!("MLT output requires building osmic with `--features mlt`"),
    })
}

fn generator_config(args: &TilesArgs, temp_parent: PathBuf) -> TileGeneratorConfig {
    TileGeneratorConfig {
        render: RenderConfig {
            min_zoom: args.zoom.min,
            max_zoom: args.zoom.max,
            extent: args.extent,
            buffer_px: args.buffer_px,
            simplify_px: args.simplify_px,
            attributes: if args.all_tags {
                AttributeMode::All
            } else {
                AttributeMode::Curated
            },
            ..RenderConfig::default()
        },
        max_tile_bytes: args.max_tile_bytes,
        memory_budget: args.memory_mb.saturating_mul(1 << 20),
        temp_dir: Some(temp_parent),
        ..TileGeneratorConfig::default()
    }
}

fn finish(
    generator: TileGenerator,
    args: &TilesArgs,
    features: u64,
    start: Instant,
) -> anyhow::Result<()> {
    let info = ArchiveInfo {
        name: args.name.clone(),
        ..ArchiveInfo::default()
    };
    let summary = generator
        .write_pmtiles(&args.output, &info, args.force)
        .with_context(|| format!("writing {}", args.output.display()))?;

    if let Some(style_path) = &args.style {
        let url = args
            .style_url
            .clone()
            .unwrap_or_else(|| format!("pmtiles://{}", args.output.display()));
        let style = osmic_style::default_style_json(&url);
        write_file(style_path, style.to_json().as_bytes(), args.force)
            .with_context(|| format!("writing style {}", style_path.display()))?;
        eprintln!("Style:   {} (source {url})", style_path.display());
    }

    println!("features        {:>14}", fmt_count(features));
    println!("tiles           {:>14}", fmt_count(summary.tiles));
    println!("archive bytes   {:>14}", fmt_bytes(summary.total_bytes));
    println!(
        "largest tile    {:>14}",
        fmt_bytes(summary.largest_tile_bytes as u64)
    );
    if summary.dropped_features > 0 {
        println!(
            "budget-limited  {:>14} tiles ({} features dropped)",
            fmt_count(summary.budget_limited_tiles),
            fmt_count(summary.dropped_features)
        );
    }
    println!("sort spill      {:>14}", fmt_bytes(summary.spilled_bytes));
    for (z, n) in &summary.tiles_per_zoom {
        println!("  z{z:<2}          {:>14}", fmt_count(*n));
    }
    println!("total time      {:>13.1}s", start.elapsed().as_secs_f64());
    Ok(())
}
