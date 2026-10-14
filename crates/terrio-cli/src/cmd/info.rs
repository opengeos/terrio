//! `terrio info`.

use terrio::info::InfoOptions;

#[derive(clap::Args)]
pub struct Args {
    /// The dataset: a path, URL, `s3://` or `gs://` object, or zip archive.
    input: String,
    /// Print JSON.
    #[arg(long)]
    json: bool,
    /// Compute band statistics.
    #[arg(long)]
    stats: bool,
    /// Only these layers of a vector dataset.
    #[arg(short, long = "layer")]
    layers: Vec<String>,
}

pub fn run(a: Args) -> terrio::Result<()> {
    let ds = if a.layers.is_empty() {
        super::quiet(|| terrio::open(&a.input))?
    } else {
        terrio::Dataset::Vector(terrio::open_vector(&a.input, &terrio::VectorOpenOptions { layers: a.layers })?)
    };
    let info = terrio::info::info(&ds, &InfoOptions { statistics: a.stats });
    if a.json {
        println!("{}", serde_json::to_string_pretty(&info).map_err(|e| terrio::Error::InvalidData(e.to_string()))?);
    } else {
        print!("{}", info.to_text());
    }
    Ok(())
}
