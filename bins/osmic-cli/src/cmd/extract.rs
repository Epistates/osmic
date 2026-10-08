//! `osmic extract`

use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, bail};
use clap::{Args, ValueEnum};

use osmic_core::BBox;
use osmic_extract::{
    ExtractConfig, Extractor, OutputOptions, deduplicate, write_csv, write_geojson, write_json,
};
use osmic_osm::{NodeStorage, TagFilter};

use super::{check_output, fmt_count};
use crate::args::{parse_bbox, parse_filter, parse_meters, parse_node_store};

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum OutputFormat {
    Csv,
    Json,
    Geojson,
}

#[derive(Debug, Args)]
pub struct ExtractArgs {
    /// Input .osm.pbf file
    pub input: PathBuf,

    /// Output file (.csv, .json or .geojson, or set --format)
    pub output: PathBuf,

    /// Keep entities matching this tag filter, e.g. "office=* shop=*"
    #[arg(long, short, value_parser = parse_filter, required_unless_present = "all_tags")]
    pub tags: Option<TagFilter>,

    /// Keep every entity (narrow with --exclude-tags and --require-name)
    #[arg(long, conflicts_with = "tags")]
    pub all_tags: bool,

    /// Drop entities matching this tag filter
    #[arg(long, value_parser = parse_filter)]
    pub exclude_tags: Option<TagFilter>,

    /// Only keep entities with a name
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    pub require_name: bool,

    /// Keep only entities inside min_lon,min_lat,max_lon,max_lat
    #[arg(long, short, value_parser = parse_bbox)]
    pub bbox: Option<BBox>,

    /// Merge same-named entities closer than this many meters (0 disables)
    #[arg(long, default_value = "100", value_parser = parse_meters)]
    pub dedup_radius: f64,

    /// Output format (default: from the output file extension)
    #[arg(long, value_enum)]
    pub format: Option<OutputFormat>,

    /// Write CSV cells exactly as in OSM, without neutralising values that
    /// spreadsheets would evaluate as formulas
    #[arg(long)]
    pub no_csv_sanitize: bool,

    /// Node location storage: sparse, dense[:MAX_ID] or file:PATH[:MAX_ID]
    #[arg(long, default_value = "sparse", value_parser = parse_node_store)]
    pub node_store: NodeStorage,

    /// Overwrite the output file
    #[arg(long)]
    pub force: bool,
}

pub fn run(args: ExtractArgs) -> anyhow::Result<()> {
    let start = Instant::now();
    if !args.input.is_file() {
        bail!("input file {} does not exist", args.input.display());
    }
    let format = match args.format {
        Some(f) => f,
        None => match args.output.extension().and_then(|e| e.to_str()) {
            Some("csv") => OutputFormat::Csv,
            Some("json") => OutputFormat::Json,
            Some("geojson") => OutputFormat::Geojson,
            other => {
                bail!("cannot infer the output format from extension {other:?}; pass --format")
            }
        },
    };
    check_output(&args.output, args.force)?;

    let include = args.tags.clone().unwrap_or_else(TagFilter::everything);
    let filter = match args.exclude_tags.clone() {
        Some(e) => TagFilter::all(vec![include, TagFilter::negate(e)]),
        None => include,
    };
    let extractor = Extractor::new(ExtractConfig {
        filter,
        require_name: args.require_name,
        bbox: args.bbox,
        node_storage: args.node_store.clone(),
    });
    let result = extractor
        .extract(&args.input)
        .with_context(|| format!("processing {}", args.input.display()))?;
    let matched = result.entities.len();
    let entities = deduplicate(result.entities, args.dedup_radius);

    let options = OutputOptions {
        overwrite: args.force,
        sanitize_csv_formulas: !args.no_csv_sanitize,
    };
    match format {
        OutputFormat::Csv => write_csv(&entities, &args.output, options),
        OutputFormat::Json => write_json(&entities, &args.output, options),
        OutputFormat::Geojson => write_geojson(&entities, &args.output, options),
    }
    .with_context(|| format!("writing {}", args.output.display()))?;

    let s = &result.stats;
    println!("nodes            {:>14}", fmt_count(s.node_count));
    println!("ways             {:>14}", fmt_count(s.way_count));
    println!("relations        {:>14}", fmt_count(s.relation_count));
    println!("matched          {:>14}", fmt_count(matched as u64));
    println!("without location {:>14}", fmt_count(s.unlocated));
    println!("after dedup      {:>14}", fmt_count(entities.len() as u64));
    println!("output           {}", args.output.display());
    println!("total time       {:>13.1}s", start.elapsed().as_secs_f64());
    Ok(())
}
