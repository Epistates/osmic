use std::path::PathBuf;
use std::time::Instant;

use clap::{Parser, Subcommand};
use osmic_extract::{
    deduplicate, write_csv, write_geojson, write_json, ExtractConfig, Extractor, TagFilter,
};
use osmic_index::RamNodeLocationStore;
use osmic_osm::pipeline::PbfProcessor;
use osmic_osm::LayerSet;
use osmic_tiles::pipeline::{TileGenerator, TileGeneratorConfig};
use osmic_tiles::pmtiles::PmTilesArchive;
use osmic_tiles::{MvtEncoder, TileEncoder};

#[derive(Parser)]
#[command(name = "osmic", about = "Osmic CLI")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Generate vector tiles from a PBF file
    GenerateTiles {
        /// Input PBF file
        pbf_file: PathBuf,

        /// Output PMTiles file
        output: PathBuf,

        /// Zoom range (e.g., "0-14")
        #[arg(long, default_value = "0-14")]
        zoom: String,

        /// Maximum expected node ID
        #[arg(long, default_value = "13000000000")]
        max_node_id: i64,

        /// MVT tile extent
        #[arg(long, default_value = "4096")]
        extent: u32,

        /// Write MapLibre style JSON to this path
        #[arg(long)]
        style: Option<PathBuf>,

        /// Comma-separated layer names to include (default: all).
        /// Available: highway,building,water,natural,landuse,railway,amenity,leisure,
        /// boundary,place,shop,tourism,office,healthcare,craft,historic
        #[arg(long)]
        layers: Option<String>,

        /// Tile encoding format
        #[arg(long, default_value = "mvt")]
        format: String,

        /// Inclusion tag filter (space-separated, OR logic) applied to
        /// each feature before it's encoded into a tile. Example:
        /// "shop=* amenity=restaurant". Omit to keep all features.
        #[arg(long)]
        tags: Option<String>,

        /// Exclude features whose tags match this filter (space-separated,
        /// OR logic). Applied after --tags.
        #[arg(long)]
        exclude_tags: Option<String>,

        /// Emit every OSM tag on each feature into the MVT output
        /// (default: only a curated whitelist of name/address/contact
        /// fields is written). Increases tile size substantially.
        #[arg(long, default_value_t = false)]
        all_tags: bool,

        /// Override the source URL embedded in the generated style JSON.
        /// By default the style references the PMTiles file via a
        /// `pmtiles://` URI (which works with the MapLibre PMTiles
        /// plugin for file:// loading). For a tile server like Martin,
        /// pass e.g. `--style-url http://localhost:3000/tires` — Martin
        /// will return TileJSON at that endpoint and MapLibre will
        /// auto-configure the vector source. Requires --style.
        #[arg(long)]
        style_url: Option<String>,

        /// Maximum RAM budget for tile generation in megabytes. When the
        /// estimated in-memory working set exceeds this limit, switches
        /// to a streaming pipeline backed by an external merge sort (one
        /// tile's features in memory at a time). Defaults to 8192 (8 GB)
        /// which is safe on most laptops; set to 0 to disable the cap.
        /// Use higher values on machines with more RAM to favor the
        /// in-memory path, which is roughly 2× faster at planet-scale
        /// but can trigger OOM.
        #[arg(long, default_value = "8192")]
        max_memory_mb: usize,
    },

    /// Inspect a PBF file and print statistics
    Inspect {
        /// Input PBF file
        pbf_file: PathBuf,

        /// Maximum expected node ID
        #[arg(long, default_value = "13000000000")]
        max_node_id: i64,
    },

    /// Extract business entities from a PBF file by tag filters
    Extract {
        /// Input PBF file
        pbf_file: PathBuf,

        /// Output file (CSV or JSON based on extension)
        output: PathBuf,

        /// Inclusion tag filter (space-separated, OR logic).
        /// Example: "office=property_management office=estate_agent shop=*".
        /// Omit (or pass --all-tags) to accept every entity.
        #[arg(long, short)]
        tags: Option<String>,

        /// Accept every entity regardless of tags. Combine with
        /// --exclude-tags and --require-name to carve down the result.
        #[arg(long, default_value_t = false)]
        all_tags: bool,

        /// Exclude entities whose tags match this filter (space-separated,
        /// OR logic — any match excludes). Example:
        /// "highway=* railway=* natural=*" drops roads, rails and nature.
        #[arg(long)]
        exclude_tags: Option<String>,

        /// Only include entities with a name tag
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        require_name: bool,

        /// Bounding box filter: min_lon,min_lat,max_lon,max_lat
        #[arg(long, short)]
        bbox: Option<String>,

        /// Deduplication radius in meters (0 to disable)
        #[arg(long, default_value = "100")]
        dedup_radius: f64,

        /// Maximum expected node ID
        #[arg(long, default_value = "13000000000")]
        max_node_id: i64,
    },

    /// Serve a PMTiles file over HTTP with a built-in MapLibre viewer.
    ///
    /// Exposes:
    ///   /                 — interactive MapLibre GL viewer auto-fit to the bbox
    ///   /tiles/{z}/{x}/{y} — vector tiles (MVT)
    ///   /style.json        — MapLibre style JSON
    ///   /metadata          — PMTiles TileJSON metadata
    ///
    /// No external tile server (Martin, tileserver-gl) required.
    Serve {
        /// PMTiles file to serve
        pmtiles: PathBuf,

        /// Host:port to bind to (default: 127.0.0.1:3000)
        #[arg(long, default_value = "127.0.0.1:3000")]
        bind: String,

        /// Cache-Control max-age for tile responses in seconds
        #[arg(long, default_value = "3600")]
        cache_max_age: u32,
    },

    /// Apply OSM replication diffs to update tiles incrementally
    Update {
        /// State directory for replication tracking
        #[arg(long)]
        state_dir: PathBuf,

        /// Replication base URL
        #[arg(
            long,
            default_value = "https://planet.openstreetmap.org/replication/minute/"
        )]
        replication_url: String,

        /// Feature store database path
        #[arg(long, default_value = "./osmic-features.redb")]
        feature_store: PathBuf,

        /// Maximum expected node ID
        #[arg(long, default_value = "13000000000")]
        max_node_id: i64,

        /// Initialize state at this sequence number (first run only)
        #[arg(long)]
        init_sequence: Option<u64>,
    },
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::GenerateTiles {
            pbf_file,
            output,
            zoom,
            max_node_id,
            extent,
            style,
            layers,
            format,
            tags,
            exclude_tags,
            all_tags,
            style_url,
            max_memory_mb,
        } => {
            let layer_set = parse_layers(layers.as_deref())?;
            let encoder = make_encoder(&format, all_tags)?;
            generate_tiles(
                &pbf_file,
                &output,
                &zoom,
                max_node_id,
                extent,
                style.as_deref(),
                &layer_set,
                encoder.as_ref(),
                tags.as_deref(),
                exclude_tags.as_deref(),
                style_url.as_deref(),
                max_memory_mb,
            )?;
        }
        Commands::Inspect {
            pbf_file,
            max_node_id,
        } => {
            inspect(&pbf_file, max_node_id)?;
        }
        Commands::Extract {
            pbf_file,
            output,
            tags,
            all_tags,
            exclude_tags,
            require_name,
            bbox,
            dedup_radius,
            max_node_id,
        } => {
            extract_entities(
                &pbf_file,
                &output,
                tags.as_deref(),
                all_tags,
                exclude_tags.as_deref(),
                require_name,
                bbox.as_deref(),
                dedup_radius,
                max_node_id,
            )?;
        }
        Commands::Serve {
            pmtiles,
            bind,
            cache_max_age,
        } => {
            serve_pmtiles(&pmtiles, &bind, cache_max_age)?;
        }
        Commands::Update {
            state_dir,
            replication_url,
            feature_store,
            max_node_id,
            init_sequence,
        } => {
            update_from_replication(
                &state_dir,
                &replication_url,
                &feature_store,
                max_node_id,
                init_sequence,
            )?;
        }
    }

    Ok(())
}

/// Serve a PMTiles archive over HTTP with a built-in MapLibre viewer.
///
/// No external dependency — this uses `osmic_serve::TileServer` (an axum
/// server) which exposes `/`, `/tiles/{z}/{x}/{y}`, `/style.json`, and
/// `/metadata`. The viewer page auto-fits to the PMTiles bbox so the
/// user sees features immediately, even for POI-only tile sets.
fn serve_pmtiles(
    pmtiles: &std::path::Path,
    bind: &str,
    cache_max_age: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    if !pmtiles.exists() {
        eprintln!("Error: PMTiles file not found: {}", pmtiles.display());
        std::process::exit(1);
    }

    let addr: std::net::SocketAddr = bind
        .parse()
        .map_err(|e| format!("Invalid --bind {bind:?}: {e}"))?;

    let config = osmic_serve::TileServerConfig {
        bind_addr: addr,
        pmtiles_path: pmtiles.to_path_buf(),
        cache_max_age,
    };

    println!("=== Osmic - Tile Server ===");
    println!("Source:  {}", pmtiles.display());
    println!("Open:    http://{addr}/");
    println!();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    runtime.block_on(async move {
        let server = osmic_serve::TileServer::new(config);
        server.serve().await
    })?;

    Ok(())
}

fn parse_zoom_range(s: &str) -> Result<(u8, u8), String> {
    if let Some((min, max)) = s.split_once('-') {
        let min: u8 = min.parse().map_err(|e| format!("Invalid min zoom: {e}"))?;
        let max: u8 = max.parse().map_err(|e| format!("Invalid max zoom: {e}"))?;
        Ok((min, max))
    } else {
        let z: u8 = s.parse().map_err(|e| format!("Invalid zoom: {e}"))?;
        Ok((z, z))
    }
}

fn parse_layers(input: Option<&str>) -> Result<LayerSet, Box<dyn std::error::Error>> {
    match input {
        None => Ok(LayerSet::all()),
        Some(s) => LayerSet::from_names(s).map_err(|e| e.into()),
    }
}

fn make_encoder(
    format: &str,
    all_tags: bool,
) -> Result<Box<dyn TileEncoder>, Box<dyn std::error::Error>> {
    match format {
        "mvt" => {
            let encoder = if all_tags {
                MvtEncoder::with_all_tags()
            } else {
                MvtEncoder::new()
            };
            Ok(Box::new(encoder))
        }
        #[cfg(feature = "mlt")]
        "mlt" => Ok(Box::new(osmic_tiles::MltEncoder)),
        #[cfg(not(feature = "mlt"))]
        "mlt" => Err(
            "MLT format requires the 'mlt' feature. Rebuild with: cargo build --features mlt"
                .into(),
        ),
        other => {
            let available = if cfg!(feature = "mlt") {
                "mvt, mlt"
            } else {
                "mvt"
            };
            Err(format!("Unsupported tile format: {other}. Available: {available}").into())
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn generate_tiles(
    pbf_file: &std::path::Path,
    output: &std::path::Path,
    zoom: &str,
    max_node_id: i64,
    extent: u32,
    style_path: Option<&std::path::Path>,
    layers: &LayerSet,
    encoder: &dyn TileEncoder,
    tags: Option<&str>,
    exclude_tags: Option<&str>,
    style_url: Option<&str>,
    max_memory_mb: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let (min_zoom, max_zoom) = parse_zoom_range(zoom)?;
    println!("=== Osmic - Tile Generator ===");
    println!("Input:  {}", pbf_file.display());
    println!("Output: {}", output.display());
    println!("Zoom:   {}-{}", min_zoom, max_zoom);
    println!("Layers: {}", layers);

    if !pbf_file.exists() {
        eprintln!("Error: PBF file not found: {}", pbf_file.display());
        std::process::exit(1);
    }

    // Step 1: Process PBF
    let total_start = Instant::now();
    let node_store = RamNodeLocationStore::create(max_node_id)?;
    let processor = PbfProcessor::new();
    let result = processor.process(pbf_file, &node_store, layers)?;

    println!(
        "\nPBF processed: {} features from {} ways in {:.1}s",
        format_number(result.features.len() as u64),
        format_number(result.stats.way_count),
        result.stats.total_duration.as_secs_f64()
    );

    // Step 1b: Optional tag filter. Features that don't match are dropped
    // before tile generation, so they never reach the encoder.
    let filtered_features =
        apply_feature_tag_filter(result.features, &result.tag_store, tags, exclude_tags)?;
    if filtered_features.len() != result.stats.feature_count as usize {
        println!(
            "Tag filter:    {} features retained ({} dropped)",
            format_number(filtered_features.len() as u64),
            format_number(
                result
                    .stats
                    .feature_count
                    .saturating_sub(filtered_features.len() as u64)
            )
        );
    }

    // Step 2: Create PMTiles archive
    let mut archive = PmTilesArchive::create(
        output,
        &result.bbox,
        min_zoom,
        max_zoom,
        encoder.tile_type(),
    )?;

    // Step 3: Generate tiles
    //
    // `max_memory_mb = 0` disables the cap (pure in-memory path, fastest
    // when you have enough RAM). Any positive value activates the
    // external-merge-sort streaming path when the estimated working set
    // exceeds the budget. 8 GB is a safe default for most laptops and
    // handles US-scale extracts without OOM.
    let memory_cap = if max_memory_mb == 0 {
        None
    } else {
        Some(max_memory_mb)
    };
    let config = TileGeneratorConfig {
        min_zoom,
        max_zoom,
        extent,
        batch_size: 10_000,
        max_memory_mb: memory_cap,
    };

    let generator = TileGenerator::new(
        &filtered_features,
        &result.tag_store,
        result.bbox,
        config,
        encoder,
    );

    let tile_count = generator.generate_all(|coord, data| archive.add_tile(coord, data))?;

    // Step 4: Finalize
    archive.finalize()?;

    println!(
        "\nGenerated {} tiles in {:.1}s",
        format_number(tile_count),
        total_start.elapsed().as_secs_f64()
    );
    println!("Output: {}", output.display());

    // Step 5: Write style if requested. The source URL in the style
    // references either a `pmtiles://` URI (default — works with the
    // in-browser PMTiles plugin) or an HTTP URL (e.g. Martin's TileJSON
    // endpoint, when `--style-url` is given).
    if let Some(style_path) = style_path {
        let source_url = match style_url {
            Some(url) => url.to_string(),
            None => format!("pmtiles://{}", output.display()),
        };
        let style = osmic_style::default_style_json(&source_url);
        std::fs::write(style_path, style.to_json())?;
        println!("Style:  {}  (source: {})", style_path.display(), source_url);
    }

    Ok(())
}

/// Apply --tags / --exclude-tags filters to the feature vector returned
/// from PBF processing. Returns the retained features. A reusable
/// `Vec<(&str, &str)>` buffer means the per-feature allocation cost is
/// amortized: no cloning of interned tag strings.
fn apply_feature_tag_filter(
    features: Vec<osmic_osm::feature::Feature>,
    tag_store: &osmic_osm::tags::TagStore,
    tags: Option<&str>,
    exclude_tags: Option<&str>,
) -> Result<Vec<osmic_osm::feature::Feature>, Box<dyn std::error::Error>> {
    let include = tags
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(TagFilter::parse);
    let exclude = exclude_tags
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(TagFilter::parse);

    if include.is_none() && exclude.is_none() {
        return Ok(features);
    }

    if let Some(t) = tags {
        println!("Tag include: {t}");
    }
    if let Some(t) = exclude_tags {
        println!("Tag exclude: {t}");
    }

    let mut scratch: Vec<(&str, &str)> = Vec::with_capacity(16);
    let retained = features
        .into_iter()
        .filter(|feat| {
            scratch.clear();
            for (k, v) in feat.tags.iter() {
                scratch.push((tag_store.resolve(*k), tag_store.resolve(*v)));
            }
            if let Some(f) = &include {
                if !f.matches_str(&scratch) {
                    return false;
                }
            }
            if let Some(f) = &exclude {
                if f.matches_str(&scratch) {
                    return false;
                }
            }
            true
        })
        .collect();
    Ok(retained)
}

fn inspect(pbf_file: &std::path::Path, max_node_id: i64) -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Osmic - PBF Inspector ===");
    println!("File: {}", pbf_file.display());

    if !pbf_file.exists() {
        eprintln!("Error: PBF file not found: {}", pbf_file.display());
        std::process::exit(1);
    }

    let file_size = std::fs::metadata(pbf_file)?.len();
    println!(
        "Size: {:.2} GB",
        file_size as f64 / (1024.0 * 1024.0 * 1024.0)
    );

    let node_store = RamNodeLocationStore::create(max_node_id)?;
    let processor = PbfProcessor::new();
    let result = processor.process(pbf_file, &node_store, &LayerSet::all())?;

    println!("\n--- Statistics ---");
    println!("Nodes:      {:>14}", format_number(result.stats.node_count));
    println!("Ways:       {:>14}", format_number(result.stats.way_count));
    println!(
        "Relations:  {:>14}",
        format_number(result.stats.relation_count)
    );
    println!(
        "Features:   {:>14}",
        format_number(result.features.len() as u64)
    );
    println!(
        "Strings:    {:>14}",
        format_number(result.tag_store.len() as u64)
    );
    println!("Bbox:       {}", result.bbox);
    println!(
        "Total time: {:>11.2}s",
        result.stats.total_duration.as_secs_f64()
    );

    // Feature breakdown
    let mut kind_counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for f in &result.features {
        *kind_counts.entry(f.kind.layer_name()).or_default() += 1;
    }
    println!("\n--- Feature Breakdown ---");
    let mut counts: Vec<_> = kind_counts.iter().collect();
    counts.sort_by(|a, b| b.1.cmp(a.1));
    for (kind, count) in counts {
        println!("  {:<12} {:>12}", kind, format_number(*count as u64));
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn extract_entities(
    pbf_file: &std::path::Path,
    output: &std::path::Path,
    tags: Option<&str>,
    all_tags: bool,
    exclude_tags: Option<&str>,
    require_name: bool,
    bbox: Option<&str>,
    dedup_radius: f64,
    max_node_id: i64,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Osmic - Entity Extractor ===");
    println!("Input:  {}", pbf_file.display());
    println!("Output: {}", output.display());

    if !pbf_file.exists() {
        eprintln!("Error: PBF file not found: {}", pbf_file.display());
        std::process::exit(1);
    }

    // Build the inclusion filter. Precedence:
    //   --all-tags             → match everything
    //   --tags "foo=*"         → parse (OR of rules)
    //   neither                → error (would match everything; protect against accidents)
    let include_filter = match (all_tags, tags) {
        (true, _) => {
            println!("Tags:   <all>");
            TagFilter::All(vec![]) // matches every entity
        }
        (false, Some(t)) if !t.trim().is_empty() => {
            println!("Tags:   {t}");
            TagFilter::parse(t)
        }
        _ => {
            eprintln!(
                "Error: must specify either --tags '<rules>' or --all-tags. \
                 Pass --all-tags to extract every entity, then narrow with \
                 --exclude-tags and/or --require-name."
            );
            std::process::exit(1);
        }
    };

    // Compose with exclusion filter, if given: final = include AND NOT exclude.
    let filter = if let Some(excl) = exclude_tags.map(|s| s.trim()).filter(|s| !s.is_empty()) {
        println!("Exclude:{excl}");
        let exclude_filter = TagFilter::parse(excl);
        TagFilter::all(vec![include_filter, TagFilter::negate(exclude_filter)])
    } else {
        include_filter
    };
    let parsed_bbox = bbox.map(|s| {
        let parts: Vec<f64> = s.split(',').filter_map(|p| p.parse().ok()).collect();
        if parts.len() != 4 {
            eprintln!("Error: bbox must be min_lon,min_lat,max_lon,max_lat");
            std::process::exit(1);
        }
        let (min_lon, min_lat, max_lon, max_lat) = (parts[0], parts[1], parts[2], parts[3]);
        if min_lon > max_lon || min_lat > max_lat {
            eprintln!(
                "Error: bbox is inverted (min > max). Expected: min_lon,min_lat,max_lon,max_lat"
            );
            std::process::exit(1);
        }
        (min_lon, min_lat, max_lon, max_lat)
    });

    if let Some(bb) = &parsed_bbox {
        println!("BBox:   {},{},{},{}", bb.0, bb.1, bb.2, bb.3);
    }

    let config = ExtractConfig {
        filter,
        require_name,
        max_node_id,
        bbox: parsed_bbox,
    };

    let extractor = Extractor::new(config);
    let result = extractor.extract(pbf_file)?;

    println!(
        "\n--- Pass Statistics ---\n\
         Nodes scanned:  {:>14}\n\
         Ways scanned:   {:>14}\n\
         Relations:      {:>14}\n\
         Entities matched:{:>13}\n\
         Pass 1 time:    {:>11.2}s\n\
         Pass 2 time:    {:>11.2}s",
        format_number(result.stats.node_count),
        format_number(result.stats.way_count),
        format_number(result.stats.relation_count),
        format_number(result.stats.matched_count),
        result.stats.pass1_duration.as_secs_f64(),
        result.stats.pass2_duration.as_secs_f64(),
    );

    let entities = if dedup_radius > 0.0 {
        let before = result.entities.len();
        let deduped = deduplicate(result.entities, dedup_radius);
        let removed = before - deduped.len();
        if removed > 0 {
            println!("Dedup removed:  {:>14}", format_number(removed as u64));
        }
        deduped
    } else {
        result.entities
    };

    // Write output based on file extension
    let ext = output.extension().and_then(|e| e.to_str()).unwrap_or("csv");

    match ext {
        "json" => write_json(&entities, output)?,
        "geojson" => write_geojson(&entities, output)?,
        _ => write_csv(&entities, output)?,
    }

    println!(
        "\nWrote {} entities to {}\nTotal time: {:.2}s",
        format_number(entities.len() as u64),
        output.display(),
        result.stats.total_duration.as_secs_f64(),
    );

    Ok(())
}

fn update_from_replication(
    state_dir: &std::path::Path,
    replication_url: &str,
    feature_store_path: &std::path::Path,
    max_node_id: i64,
    init_sequence: Option<u64>,
) -> Result<(), Box<dyn std::error::Error>> {
    use osmic_osm::tags::TagStore;
    use osmic_tiles::TileGeneratorConfig;

    println!("=== Osmic - Replication Update ===");

    // Load or initialize state
    let mut state = if let Some(seq) = init_sequence {
        println!("Initializing replication state at sequence {seq}");
        let s = osmic_repl::ReplicationState::init(replication_url, seq);
        s.save(state_dir)?;
        s
    } else {
        osmic_repl::ReplicationState::load(state_dir)?
    };

    println!("Current sequence: {}", state.sequence_number);

    // Open stores
    let node_store = RamNodeLocationStore::create(max_node_id)?;
    let store = osmic_repl::FeatureStore::open(feature_store_path)?;
    let tag_store = TagStore::new();
    let layers = LayerSet::all();
    let config = TileGeneratorConfig::default();

    // Download next change file
    let url = state.next_osc_url();
    println!("Downloading: {url}");

    let response = reqwest::blocking::get(&url)?;
    if !response.status().is_success() {
        eprintln!("No new changes available (HTTP {})", response.status());
        return Ok(());
    }
    let data = response.bytes()?;

    // Parse changes
    let changes = osmic_repl::osc::parse_osc_gz_bytes(&data)?;
    println!("Parsed {} changes", format_number(changes.len() as u64));

    // Apply changes
    let dirty =
        osmic_repl::apply_changes(&changes, &store, &node_store, &tag_store, &layers, &config)?;

    println!(
        "Dirty tiles: {} (across zoom {}-{})",
        format_number(dirty.len() as u64),
        config.min_zoom,
        config.max_zoom
    );

    // Update state
    state.sequence_number += 1;
    state.save(state_dir)?;
    println!("State updated to sequence {}", state.sequence_number);

    Ok(())
}

fn format_number(n: u64) -> String {
    let s = n.to_string();
    let mut result = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    result.chars().rev().collect()
}
