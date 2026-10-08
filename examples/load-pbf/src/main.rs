//! Load an OSM PBF file into memory, build a spatial index over the
//! features and run a sample query.
//!
//! ```sh
//! cargo run --release -p load-pbf -- extract.osm.pbf
//! ```

use std::path::PathBuf;
use std::time::Instant;

use clap::Parser;

use osmic_core::BBox;
use osmic_osm::{FeatureIndex, PbfProcessor, PipelineConfig};

#[derive(Parser)]
#[command(name = "load-pbf", about = "Load an OSM PBF file and print statistics")]
struct Args {
    /// Path to the .osm.pbf file
    pbf_file: PathBuf,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let args = Args::parse();

    let start = Instant::now();
    let data = PbfProcessor::new(PipelineConfig::default()).process(&args.pbf_file)?;
    let index_start = Instant::now();
    let index = FeatureIndex::build(&data.features);
    let index_secs = index_start.elapsed().as_secs_f64();

    let s = &data.stats;
    println!("nodes              {:>14}", s.node_count);
    println!("ways               {:>14}", s.way_count);
    println!("relations          {:>14}", s.relation_count);
    println!("features           {:>14}", data.features.len());
    println!("r-tree entries     {:>14}", index.len());
    println!("interned strings   {:>14}", data.tag_store.len());
    println!("bounding box       {}", data.bbox);
    println!(
        "pass 1             {:>13.2}s",
        s.pass1_duration.as_secs_f64()
    );
    println!(
        "pass 2             {:>13.2}s",
        s.pass2_duration.as_secs_f64()
    );
    println!("spatial index      {index_secs:>13.2}s");
    println!(
        "total              {:>13.2}s",
        start.elapsed().as_secs_f64()
    );

    let c = data.bbox.center();
    let query = BBox::new(c.lon - 0.01, c.lat - 0.01, c.lon + 0.01, c.lat + 0.01);
    println!(
        "features within 0.01° of the centre ({:.4}, {:.4}): {}",
        c.lon,
        c.lat,
        index.query_bbox(&query).count()
    );
    Ok(())
}
