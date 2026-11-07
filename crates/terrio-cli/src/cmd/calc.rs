// Written to match the behavior of:
//   GDAL v3.13.3:
//     doc/source/programs/gdal_calc.rst
// No code is translated from these sources.

//! `terrio calc`.

use super::rasterout::{OutputArgs, open};
use std::ffi::OsString;
use terrio::Error;
use terrio::ops::calc::{CalcExtent, CalcInput, CalcNodata, CalcOptions, calc};
use terrio::ops::parse_data_type;

#[derive(clap::Args)]
#[command(after_help = "Inputs are also given as in gdal_calc.py: `-A a.tif -B b.tif --A_band 2`.\n\n\
Example: terrio calc -A nir.tif -B red.tif --calc \"(A.astype(float32) - B) / (A.astype(float32) + B)\" ndvi.tif")]
pub struct Args {
    /// The output path (or --outfile).
    output: Option<String>,
    /// The output path.
    #[arg(long, conflicts_with = "output")]
    outfile: Option<String>,
    /// An expression in NumPy syntax over the inputs; repeat for several output bands.
    #[arg(long = "calc", required = true)]
    calc: Vec<String>,
    /// An input raster, as NAME=PATH (`-A PATH` is the same as `--input A=PATH`).
    #[arg(long = "input", value_name = "NAME=PATH")]
    inputs: Vec<String>,
    /// The band of an input, as NAME=N (`--A_band N` is the same as `--input-band A=N`).
    #[arg(long = "input-band", value_name = "NAME=N")]
    input_bands: Vec<String>,
    /// The output type (Byte, Int16, UInt16, Int32, UInt32, Float32, Float64...). By default the
    /// smallest type holding all the inputs' types.
    #[arg(long = "type", alias = "ot")]
    output_type: Option<String>,
    /// The output nodata value, or `none`. By default a value depending on the output type.
    #[arg(long = "NoDataValue", alias = "nodata", allow_hyphen_values = true)]
    nodata: Option<String>,
    /// Ignore the inputs' nodata values.
    #[arg(long = "hideNoData", alias = "hide-nodata")]
    hide_nodata: bool,
    /// How to handle inputs with different extents: ignore (the default; sizes must match),
    /// fail, union or intersect.
    #[arg(long, conflicts_with = "projwin")]
    extent: Option<String>,
    /// Compute over this window, aligned on the first input's pixels.
    #[arg(long, num_args = 4, value_names = ["ULX", "ULY", "LRX", "LRY"], allow_negative_numbers = true)]
    projwin: Option<Vec<f64>>,
    /// Process every band of this input, giving one output band each.
    #[arg(long = "allBands", alias = "all-bands", value_name = "NAME")]
    all_bands: Option<String>,
    /// Fail when the inputs' CRSs differ.
    #[arg(long = "projectionCheck", alias = "projection-check")]
    projection_check: bool,
    /// Accepted for compatibility with gdal_calc.py: outputs are always overwritten.
    #[arg(long, hide = true)]
    overwrite: bool,
    /// Accepted for compatibility with gdal_calc.py.
    #[arg(long, hide = true)]
    quiet: bool,
    #[command(flatten)]
    out: OutputArgs,
}

/// Rewrites gdal_calc.py's input options (`-A path`, `--A_band n`) into terrio's (`--input
/// A=path`, `--input-band A=n`) when the subcommand is `calc`.
pub fn rewrite_args(args: Vec<OsString>) -> Vec<OsString> {
    if args.iter().skip(1).find_map(|a| a.to_str().filter(|s| !s.starts_with('-'))) != Some("calc") {
        return args;
    }
    let strs: Vec<&str> = args.iter().filter_map(|a| a.to_str()).collect();
    let mut calc_exprs = Vec::new();
    let mut has_f_band_or_all = false;
    let mut i = 0;
    while i < strs.len() {
        let s = strs[i];
        if s == "--" {
            break;
        }
        if s == "--calc" {
            if let Some(&expr) = strs.get(i + 1) {
                calc_exprs.push(expr);
            }
            i += 2;
            continue;
        }
        if let Some(expr) = s.strip_prefix("--calc=") {
            calc_exprs.push(expr);
        } else if s.starts_with("--f_band")
            || s.starts_with("--f-band")
            || s == "--allBands=f"
            || s == "--all-bands=f"
            || ((s == "--allBands" || s == "--all-bands") && strs.get(i + 1) == Some(&"f"))
        {
            has_f_band_or_all = true;
        }
        i += 1;
    }
    let f_is_input =
        has_f_band_or_all || calc_exprs.iter().any(|e| e.split(|c: char| !c.is_alphanumeric() && c != '_').any(|tok| tok == "f"));
    let mut out = Vec::with_capacity(args.len());
    let mut it = args.into_iter().peekable();
    while let Some(arg) = it.next() {
        let Some(s) = arg.to_str() else {
            out.push(arg);
            continue;
        };
        if s == "--" {
            out.push(arg);
            out.extend(it);
            break;
        }
        if matches!(
            s,
            "--outfile"
                | "--calc"
                | "--input"
                | "--input-band"
                | "--type"
                | "--ot"
                | "--NoDataValue"
                | "--nodata"
                | "--extent"
                | "--allBands"
                | "--all-bands"
                | "--format"
                | "--of"
                | "--compress"
                | "--tile-size"
                | "--overview-resampling"
                | "--co"
        ) {
            out.push(arg);
            if let Some(val) = it.next() {
                out.push(val);
            }
            continue;
        }
        if s == "--projwin" {
            out.push(arg);
            for _ in 0..4 {
                if let Some(val) = it.next() {
                    out.push(val);
                }
            }
            continue;
        }
        let letter = s.len() == 2
            && s.starts_with('-')
            && s.as_bytes()[1].is_ascii_alphabetic()
            && !matches!(s, "-h" | "-V")
            && (s != "-f" || f_is_input);
        let next_is_value = it.peek().and_then(|n| n.to_str()).is_some_and(|n| !n.starts_with('-'));
        if letter && next_is_value {
            let path = it.next().expect("peeked");
            out.push("--input".into());
            let mut v = OsString::from(format!("{}=", &s[1..]));
            v.push(path);
            out.push(v);
            continue;
        }
        if let Some(rest) = s.strip_prefix("--")
            && let Some((name, value)) = rest.split_once('=').map_or_else(|| Some((rest, None)), |(n, v)| Some((n, Some(v))))
            && let Some(letter) = name.strip_suffix("_band").or_else(|| name.strip_suffix("-band"))
            && letter.len() == 1
            && letter.as_bytes()[0].is_ascii_alphabetic()
        {
            let value = match value {
                Some(v) => v.to_string(),
                None => it.next().and_then(|v| v.into_string().ok()).unwrap_or_default(),
            };
            out.push("--input-band".into());
            out.push(format!("{letter}={value}").into());
            continue;
        }
        out.push(arg);
    }
    out
}

pub fn run(a: Args) -> terrio::Result<()> {
    let output = a.output.or(a.outfile).ok_or_else(|| Error::InvalidArgument("give the output path".into()))?;
    let pair = |s: &str, what: &str| -> terrio::Result<(String, String)> {
        let (n, v) = s.split_once('=').ok_or_else(|| Error::InvalidArgument(format!("{what} '{s}' is not NAME=VALUE")))?;
        let n = n.trim();
        if n.is_empty() || !n.chars().all(|c| c.is_alphanumeric() || c == '_') || n.starts_with(|c: char| c.is_ascii_digit()) {
            return Err(Error::InvalidArgument(format!("invalid input name '{n}'")));
        }
        Ok((n.to_string(), v.to_string()))
    };
    let mut named: Vec<(String, String)> = a.inputs.iter().map(|s| pair(s, "input")).collect::<terrio::Result<_>>()?;
    // gdal_calc.py takes single-letter inputs in the order a-z, A-Z.
    let rank = |n: &str| -> usize {
        match n.as_bytes() {
            [c] if c.is_ascii_lowercase() => (c - b'a') as usize,
            [c] if c.is_ascii_uppercase() => 26 + (c - b'A') as usize,
            _ => 52,
        }
    };
    named.sort_by_key(|(n, _)| rank(n));
    let mut bands = Vec::new();
    for s in &a.input_bands {
        let (n, v) = pair(s, "input band")?;
        let b: usize = v.trim().parse().map_err(|_| Error::InvalidArgument(format!("invalid band '{v}' for input {n}")))?;
        if !named.iter().any(|(m, _)| *m == n) {
            return Err(Error::InvalidArgument(format!("a band is given for input {n}, which is not an input")));
        }
        bands.push((n, b));
    }
    let datasets = named.iter().map(|(_, p)| open(p)).collect::<terrio::Result<Vec<_>>>()?;
    let inputs: Vec<CalcInput> = named
        .iter()
        .zip(&datasets)
        .map(|((n, _), ds)| CalcInput { name: n.clone(), ds, band: bands.iter().rev().find(|(m, _)| m == n).map_or(1, |(_, b)| *b) })
        .collect();
    let nodata = match a.nodata.as_deref() {
        None => CalcNodata::Default,
        Some(s) if s.eq_ignore_ascii_case("none") => CalcNodata::None,
        Some(s) => CalcNodata::Value(s.parse().map_err(|_| Error::InvalidArgument(format!("invalid float value for NoDataValue: {s}")))?),
    };
    let o = CalcOptions {
        calc: a.calc,
        output_type: a.output_type.as_deref().map(parse_data_type).transpose()?,
        nodata,
        hide_nodata: a.hide_nodata,
        extent: a.extent.as_deref().map(CalcExtent::parse).transpose()?.unwrap_or_default(),
        projwin: a.projwin.map(|v| (v[0], v[1], v[2], v[3])),
        all_bands: a.all_bands,
        projection_check: a.projection_check,
    };
    let _ = (a.overwrite, a.quiet);
    let out = calc(&inputs, &o)?;
    a.out.write(&out, &output)
}

#[cfg(test)]
mod tests {
    use super::rewrite_args;

    #[test]
    fn rewrites_gdal_calc_inputs() {
        let args: Vec<_> =
            ["terrio", "calc", "-A", "a.tif", "--B_band=2", "-B", "b.tif", "--calc", "A+B", "--NoDataValue", "-1", "out.tif"]
                .iter()
                .map(Into::into)
                .collect();
        let out: Vec<String> = rewrite_args(args).into_iter().map(|s| s.into_string().unwrap()).collect();
        assert_eq!(
            out,
            [
                "terrio",
                "calc",
                "--input",
                "A=a.tif",
                "--input-band",
                "B=2",
                "--input",
                "B=b.tif",
                "--calc",
                "A+B",
                "--NoDataValue",
                "-1",
                "out.tif"
            ]
        );

        // `-f` is kept as `--format` unless `f` is referenced in `--calc` / `--f_band` / `--allBands`.
        let fmt_args: Vec<_> = ["terrio", "calc", "-A", "a.tif", "-f", "COG", "--calc", "-A", "out.tif"].iter().map(Into::into).collect();
        let fmt_out: Vec<String> = rewrite_args(fmt_args).into_iter().map(|s| s.into_string().unwrap()).collect();
        assert_eq!(fmt_out, ["terrio", "calc", "--input", "A=a.tif", "-f", "COG", "--calc", "-A", "out.tif"]);

        let var_f_args: Vec<_> = ["terrio", "calc", "-f", "a.tif", "--calc", "f + 1", "out.tif"].iter().map(Into::into).collect();
        let var_f_out: Vec<String> = rewrite_args(var_f_args).into_iter().map(|s| s.into_string().unwrap()).collect();
        assert_eq!(var_f_out, ["terrio", "calc", "--input", "f=a.tif", "--calc", "f + 1", "out.tif"]);
    }
}
