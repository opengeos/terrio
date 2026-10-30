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
    /// Selections run in this order: --isel, --slice, --isel-axis, --slice-axis,
    /// then --transpose, regardless of their order on the command line.
    #[arg(long, value_name = "DIM=INDEX")]
    isel: Vec<String>,
    /// Positive-stride range after indexing: dimension=start,count[,step]. May be repeated.
    #[arg(long, value_name = "DIM=START,COUNT[,STEP]")]
    slice: Vec<String>,
    /// Select/remove a zero-based axis after all named selections and earlier
    /// --isel-axis entries: axis=index. Useful for repeated names.
    #[arg(long, value_name = "AXIS=INDEX")]
    isel_axis: Vec<String>,
    /// Slice a zero-based axis after all indexing: axis=start,count[,step].
    #[arg(long, value_name = "AXIS=START,COUNT[,STEP]")]
    slice_axis: Vec<String>,
    /// Maximum JSON element count; 0 explicitly permits an unlimited selection.
    #[arg(long, default_value_t = 1_000_000)]
    max_elements: usize,
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
#[derive(clap::Args)]
pub struct MosaicArgs {
    /// Input netCDF/HDF5 files or Zarr stores, in priority order (last wins).
    #[arg(required = true, num_args = 1..)]
    inputs: Vec<String>,
    /// New .nc file or .zarr store; must not exist.
    output: String,
    /// Array name or full path to mosaic; repeat to select several arrays.
    #[arg(long = "array")]
    arrays: Vec<String>,
}
pub fn mosaic(args: MosaicArgs) -> Result<()> {
    let datasets = args.inputs.iter().map(|p| multidim::open(p)).collect::<Result<Vec<_>>>()?;
    multidim::mosaic(&datasets, &multidim::MosaicOptions { arrays: args.arrays })?.write(&args.output)
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
    for selection in &args.isel_axis {
        let (axis, index) = selection.split_once('=').ok_or_else(|| invalid("--isel-axis requires axis=index"))?;
        view = view.isel_axis(
            axis.parse().map_err(|_| invalid("axis must be a zero-based integer"))?,
            index.parse().map_err(|_| invalid("axis index must be a nonnegative integer"))?,
        )?;
    }
    for selection in &args.slice_axis {
        let (axis, range) = selection.split_once('=').ok_or_else(|| invalid("--slice-axis requires axis=start,count[,step]"))?;
        let sizes: Vec<usize> =
            range.split(',').map(|v| v.parse().map_err(|_| invalid("slice values must be nonnegative integers"))).collect::<Result<_>>()?;
        if !(2..=3).contains(&sizes.len()) {
            return Err(invalid("--slice-axis requires axis=start,count[,step]"));
        }
        view = view.slice_axis(
            axis.parse().map_err(|_| invalid("axis must be a zero-based integer"))?,
            sizes[0],
            sizes[1],
            sizes.get(2).copied().unwrap_or(1),
        )?;
    }
    if !args.transpose.is_empty() {
        let dimensions = view.dimensions();
        let permutation: Vec<_> = args.transpose.iter().map(|name| transpose_axis(&dimensions, name)).collect::<Result<_>>()?;
        view = view.transpose(&permutation)?;
    }
    let elements =
        view.shape().into_iter().try_fold(1usize, |n, size| n.checked_mul(size).ok_or_else(|| invalid("selection size overflows")))?;
    if args.max_elements != 0 && elements > args.max_elements {
        return Err(invalid(format!(
            "selection has {elements} elements, exceeding --max-elements {}; use --slice/--isel or increase --max-elements (0 permits unlimited)",
            args.max_elements
        )));
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

fn transpose_axis(dimensions: &[multidim::Dimension], name: &str) -> Result<usize> {
    let mut matches = dimensions.iter().enumerate().filter(|(_, d)| d.name == name || d.path == name);
    let index = matches.next().map(|(i, _)| i).ok_or_else(|| invalid(format!("unknown remaining dimension '{name}'")))?;
    if matches.next().is_some() {
        return Err(invalid(format!(
            "ambiguous transpose dimension '{name}'; use a unique full dimension path or --isel-axis to remove repeated axes"
        )));
    }
    Ok(index)
}

#[cfg(test)]
mod tests {
    #[test]
    fn transpose_rejects_ambiguous_names_and_accepts_unique_paths() {
        let make = |path: &str| terrio::multidim::Dimension {
            name: "x".into(),
            path: path.into(),
            size: 2,
            unlimited: false,
            indexing_variable: None,
        };
        let dimensions = [make("/a/x"), make("/b/x")];
        assert!(super::transpose_axis(&dimensions, "x").unwrap_err().to_string().contains("ambiguous transpose dimension"));
        assert_eq!(super::transpose_axis(&dimensions, "/b/x").unwrap(), 1);
        let repeated = [make("/x"), make("/x")];
        assert!(super::transpose_axis(&repeated, "/x").is_err());
    }
}
