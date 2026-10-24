//! Native multidimensional inspection, hyperslab reading and scientific-format snapshots.
use terrio::{Error, Result, multidim};

#[derive(clap::Args)]
pub struct InfoArgs {
    /// netCDF/HDF5 file or local/remote Zarr store.
    input: String,
    /// Print the complete group/dimension/array metadata as JSON.
    #[arg(long)]
    json: bool,
}
#[derive(clap::Args)]
pub struct ReadArgs {
    /// netCDF/HDF5 file or local/remote Zarr store.
    input: String,
    /// Absolute array path, such as /science/temperature.
    #[arg(long)]
    array: String,
    /// Select and remove an axis: dimension=index. May be repeated.
    #[arg(long, value_name = "DIM=INDEX")]
    isel: Vec<String>,
    /// Positive-stride range after indexing: dimension=start,count[,step]. May be repeated.
    #[arg(long, value_name = "DIM=START,COUNT[,STEP]")]
    slice: Vec<String>,
    /// Reorder all remaining dimensions, by their comma-separated names.
    #[arg(long, value_delimiter = ',')]
    transpose: Vec<String>,
}
#[derive(clap::Args)]
pub struct ConvertArgs {
    /// Source netCDF/HDF5 file or local/remote Zarr store.
    input: String,
    /// New .nc (netCDF-4) file or .zarr (Zarr v2) store; must not exist.
    output: String,
}
fn invalid(message: impl std::fmt::Display) -> Error {
    Error::InvalidArgument(format!("multidim: {message}"))
}
pub fn info(args: InfoArgs) -> Result<()> {
    let ds = multidim::open(&args.input)?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&ds).map_err(invalid)?);
    } else {
        println!("Driver: {}", ds.driver);
        fn group(g: &multidim::Group) {
            println!("Group {}", g.path);
            for d in &g.dimensions {
                println!("  Dimension {}: {}{}", d.name, d.size, if d.unlimited { " (unlimited)" } else { "" });
            }
            for a in &g.arrays {
                println!("  Array {}: {} {:?}", a.info.path, terrio::info::gdal_type_name(a.info.data_type), a.shape());
            }
            for a in &g.unsupported_arrays {
                println!("  Unsupported array {}: {} {:?}", a.path, a.data_type, a.shape);
            }
            for child in &g.groups {
                group(child);
            }
        }
        group(&ds.root);
    }
    Ok(())
}
pub fn read(args: ReadArgs) -> Result<()> {
    let ds = multidim::open(&args.input)?;
    let a = ds.array(&args.array)?;
    let mut view = a.view();
    for selection in &args.isel {
        let (name, index) = selection.split_once('=').ok_or_else(|| invalid("--isel requires dimension=index"))?;
        view = view.isel(name, index.parse().map_err(|_| invalid("axis index must be a nonnegative integer"))?)?;
    }
    for selection in &args.slice {
        let (name, range) = selection.split_once('=').ok_or_else(|| invalid("--slice requires dimension=start,count[,step]"))?;
        let values: Vec<usize> =
            range.split(',').map(|v| v.parse().map_err(|_| invalid("slice values must be nonnegative integers"))).collect::<Result<_>>()?;
        if !matches!(values.len(), 2 | 3) {
            return Err(invalid("--slice requires dimension=start,count[,step]"));
        }
        view = view.slice(name, values[0], values[1], values.get(2).copied().unwrap_or(1))?;
    }
    if !args.transpose.is_empty() {
        let dimensions = view.dimensions();
        let permutation: Vec<_> = args
            .transpose
            .iter()
            .map(|name| {
                dimensions
                    .iter()
                    .position(|d| d.name == *name || d.path == *name)
                    .ok_or_else(|| invalid(format!("unknown remaining dimension '{name}'")))
            })
            .collect::<Result<_>>()?;
        view = view.transpose(&permutation)?;
    }
    let buffer = view.read()?;
    println!(
        "{}",
        serde_json::to_string(
            &serde_json::json!({"array":a.info.path,"data_type":terrio::info::gdal_type_name(buffer.data_type),"shape":buffer.shape,"dimensions":view.dimensions(),"values":buffer.values()?})
        ).map_err(invalid)?
    );
    Ok(())
}
pub fn convert(args: ConvertArgs) -> Result<()> {
    multidim::open(&args.input)?.write(&args.output)
}
