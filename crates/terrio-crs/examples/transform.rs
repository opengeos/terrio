//! Run with `cargo run -p terrio-crs --example transform`.
use terrio_crs::{SpatialRef, Transformer};

fn main() -> terrio_crs::Result<()> {
    let source = SpatialRef::parse("EPSG:4326")?;
    let target = SpatialRef::parse("EPSG:32613")?;
    let transformer = Transformer::new(&source, &target)?;
    let (easting, northing) = transformer.transform(-105.0, 40.0)?;
    println!("{easting} {northing}");
    Ok(())
}
