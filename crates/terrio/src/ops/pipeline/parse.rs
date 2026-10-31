//! Validated pipeline steps. No commands are sent to a shell.
use super::{Error, InputKind, Result};
use crate::crs::SpatialRef;
use crate::ops::{Resampling, as_features, rasterize, vector_filter};
use std::collections::BTreeMap;

#[derive(Debug, Clone)]
pub(super) struct WriteSpec {
    pub path: String,
    pub format: Option<String>,
    pub overwrite: bool,
    pub append: bool,
    pub layer: Option<String>,
}
impl WriteSpec {
    pub fn definition(&self) -> bool {
        self.format.as_deref().map_or_else(|| self.path.to_ascii_lowercase().ends_with(".gdalg.json"), |f| f.eq_ignore_ascii_case("GDALG"))
    }
}
#[derive(Debug, Clone)]
pub(super) enum Step {
    Read { source: String, layers: Vec<String> },
    Filter(vector_filter::VectorTranslateOptions),
    Select(Vec<String>),
    Clip { bbox: [f64; 4] },
    Reproject { crs: SpatialRef, resampling: Option<Resampling>, size: Option<(usize, usize)>, resolution: Option<(f64, f64)> },
    Resize { size: Option<(String, String)>, resolution: Option<(f64, f64)>, resampling: Resampling },
    SetType(wbraster::DataType),
    Buffer { distance: f64, quadrant_segments: usize },
    Simplify(f64),
    Segmentize(f64),
    SwapXY,
    Dissolve,
    AsFeatures(as_features::AsFeaturesOptions),
    Rasterize(rasterize::RasterizeOptions),
    Materialize(Option<WriteSpec>),
    Write(WriteSpec),
}
#[derive(Debug, Clone)]
pub(super) struct Block {
    pub argv: Vec<String>,
    pub step: Step,
}

pub(super) fn parse(tokens: &[String]) -> Result<(InputKind, Vec<Block>)> {
    let mut words = tokens;
    if words.first().is_some_and(|s| s == "gdal" || s == "terrio") {
        words = &words[1..];
    }
    let mut kind = InputKind::Auto;
    if let Some(s) = words.first() {
        if s == "raster" {
            kind = InputKind::Raster;
            words = &words[1..];
        } else if s == "vector" {
            kind = InputKind::Vector;
            words = &words[1..];
        }
    }
    if words.first().is_some_and(|s| s == "pipeline") {
        words = &words[1..];
    }
    if words.first().is_some_and(|s| s == "!") {
        words = &words[1..];
    }
    if words.is_empty() {
        return Err(bad("empty pipeline"));
    }
    let mut blocks = vec![];
    for argv in words.split(|s| s == "!") {
        if argv.is_empty() {
            return Err(bad("empty step between ! separators"));
        }
        let step = parse_step(argv)?;
        blocks.push(Block { argv: argv.to_vec(), step });
    }
    if !matches!(blocks[0].step, Step::Read { .. }) {
        return Err(bad("pipeline must start with read"));
    }
    for (i, b) in blocks.iter().enumerate() {
        if i > 0 && matches!(b.step, Step::Read { .. }) {
            return Err(bad("read may only be the first step"));
        }
        if i + 1 < blocks.len() && matches!(b.step, Step::Write(_)) {
            return Err(bad("write must be the final step"));
        }
    }
    Ok((kind, blocks))
}
fn parse_step(argv: &[String]) -> Result<Step> {
    let mut a = Args::new(&argv[1..])?;
    let step = match argv[0].as_str() {
        "read" => Step::Read { source: a.required_pos("input")?, layers: a.many("layer") },
        "write" => Step::Write(a.write()?),
        "materialize" => {
            let output = a.one("output")?;
            if let Some(path) = output {
                a.values.insert("output".into(), vec![path]);
                Step::Materialize(Some(a.write()?))
            } else {
                Step::Materialize(None)
            }
        }
        "filter" => {
            let where_clause = a.one("where")?;
            let bbox = a.array::<4>("bbox")?;
            Step::Filter(vector_filter::VectorTranslateOptions {
                where_clause,
                spatial_filter: bbox.map(|v| (v[0], v[1], v[2], v[3])),
                ..Default::default()
            })
        }
        "select" => {
            let mut values = a.many("band");
            values.extend(a.many("fields"));
            values.append(&mut a.pos);
            let values = values.into_iter().flat_map(|s| s.split(',').map(str::to_string).collect::<Vec<_>>()).collect::<Vec<_>>();
            if values.is_empty() {
                return Err(bad("select needs bands or fields"));
            }
            Step::Select(values)
        }
        "clip" => {
            let bbox = a.array::<4>("bbox")?.ok_or_else(|| bad("clip needs --bbox xmin,ymin,xmax,ymax"))?;
            if bbox[0] >= bbox[2] || bbox[1] >= bbox[3] {
                return Err(bad("clip bbox must have positive width and height"));
            }
            Step::Clip { bbox }
        }
        "reproject" => {
            let crs = SpatialRef::parse(&a.one("output-crs")?.ok_or_else(|| bad("reproject needs --output-crs"))?)?;
            let resampling = a.one("resampling")?.map(|v| Resampling::parse(&v)).transpose()?;
            let size = a.usize_pair("size")?;
            let resolution = a.pair("resolution")?;
            exclusive_grid(size.is_some(), resolution)?;
            Step::Reproject { crs, resampling, size, resolution }
        }
        "resize" => {
            let size = a
                .one("size")?
                .map(|s| {
                    let v = parts(&s);
                    if v.len() != 2 {
                        return Err(bad("size needs width,height"));
                    }
                    for s in &v {
                        crate::ops::translate::Size::parse(s)?;
                    }
                    Ok((v[0].clone(), v[1].clone()))
                })
                .transpose()?;
            let resolution = a.pair("resolution")?;
            exclusive_grid(size.is_some(), resolution)?;
            if size.is_none() && resolution.is_none() {
                return Err(bad("resize needs --size or --resolution"));
            }
            Step::Resize { size, resolution, resampling: resampling(&mut a)? }
        }
        "set-type" => Step::SetType(crate::ops::parse_data_type(
            &a.one("output-data-type")?.ok_or_else(|| bad("set-type requires --output-data-type"))?,
        )?),
        "buffer" => {
            let distance = a.required_pos("distance")?.parse::<f64>().map_err(|_| bad("invalid buffer distance"))?;
            if !distance.is_finite() {
                return Err(bad("buffer distance must be finite"));
            }
            let quadrant_segments = a
                .one("quadrant-segments")?
                .map(|v| v.parse::<usize>().map_err(|_| bad("invalid quadrant-segments")))
                .transpose()?
                .unwrap_or(8);
            if !(1..=4096).contains(&quadrant_segments) {
                return Err(bad("quadrant-segments must be 1..4096"));
            }
            Step::Buffer { distance, quadrant_segments }
        }
        "simplify" => Step::Simplify(a.positive("tolerance", true)?),
        "segmentize" => Step::Segmentize(a.positive("max-length", false)?),
        "swap-xy" => Step::SwapXY,
        "dissolve" => Step::Dissolve,
        "as-features" => {
            let options = as_features::AsFeaturesOptions {
                geometry: as_features::PixelGeometry::parse(&a.one("geometry-type")?.unwrap_or_else(|| "none".into()))?,
                skip_nodata: a.flag("skip-nodata")?,
                include_xy: a.flag("include-xy")?,
                include_row_col: a.flag("include-row-col")?,
                layer_name: a.one("output-layer")?.unwrap_or_else(|| "pixels".into()),
                ..Default::default()
            };
            Step::AsFeatures(options)
        }
        "rasterize" => {
            let size = a.usize_pair("size")?;
            let resolution = a.pair("resolution")?;
            exclusive_grid(size.is_some(), resolution)?;
            if size.is_none() && resolution.is_none() {
                return Err(bad("rasterize needs --size or --resolution"));
            }
            let extent = a.array::<4>("extent")?.map(|v| (v[0], v[1], v[2], v[3]));
            if extent.is_some_and(|e| e.0 >= e.2 || e.1 >= e.3) {
                return Err(bad("rasterize extent must have positive width and height"));
            }
            let burn_values =
                a.many("burn").iter().map(|v| v.parse::<f64>().map_err(|_| bad("invalid burn value"))).collect::<Result<Vec<_>>>()?;
            let attribute = a.one("attribute-name")?;
            let all_touched = a.flag("all-touched")?;
            let data_type = a.one("output-data-type")?.map(|v| crate::ops::parse_data_type(&v)).transpose()?;
            let nodata = a.one("nodata")?.map(|v| v.parse::<f64>().map_err(|_| bad("invalid nodata"))).transpose()?;
            Step::Rasterize(rasterize::RasterizeOptions {
                size,
                resolution,
                extent,
                burn_values,
                attribute,
                all_touched,
                data_type,
                nodata,
                ..Default::default()
            })
        }
        name => return Err(bad(&format!("unsupported pipeline step {name:?}"))),
    };
    a.finish()?;
    Ok(step)
}
fn resampling(a: &mut Args) -> Result<Resampling> {
    a.one("resampling")?.map(|v| Resampling::parse(&v)).transpose().map(|v| v.unwrap_or_default())
}
fn exclusive_grid(size: bool, resolution: Option<(f64, f64)>) -> Result<()> {
    if size && resolution.is_some() {
        return Err(bad("size and resolution are mutually exclusive"));
    }
    if resolution.is_some_and(|(x, y)| x <= 0. || y <= 0. || !x.is_finite() || !y.is_finite()) {
        return Err(bad("resolution must be finite and positive"));
    }
    Ok(())
}
fn parts(s: &str) -> Vec<String> {
    s.split([',', ' ']).filter(|s| !s.is_empty()).map(str::to_string).collect()
}
fn bad(s: &str) -> Error {
    Error::InvalidArgument(format!("pipeline: {s}"))
}
struct Args {
    values: BTreeMap<String, Vec<String>>,
    pos: Vec<String>,
}
impl Args {
    fn new(tokens: &[String]) -> Result<Self> {
        let mut out = Self { values: BTreeMap::new(), pos: vec![] };
        let mut i = 0;
        while i < tokens.len() {
            let token = &tokens[i];
            i += 1;
            if !token.starts_with('-') || token.parse::<f64>().is_ok() {
                out.pos.push(token.clone());
                continue;
            }
            let (key, value) = token
                .trim_start_matches('-')
                .split_once('=')
                .map_or((token.trim_start_matches('-'), None), |(k, v)| (k, Some(v.to_string())));
            let key = match key {
                "i" => "input",
                "o" => "output",
                "f" | "of" | "output-format" => "format",
                "l" | "input-layer" => "layer",
                "b" => "band",
                "d" | "dst-crs" => "output-crs",
                "r" => "resampling",
                "ot" | "datatype" => "output-data-type",
                k => k,
            };
            let flag = matches!(key, "overwrite" | "append" | "skip-nodata" | "include-xy" | "include-row-col" | "all-touched");
            let mut value = match value {
                Some(v) => v,
                None if flag => "true".into(),
                None => {
                    let v = tokens.get(i).ok_or_else(|| bad(&format!("missing value for --{key}")))?.clone();
                    i += 1;
                    v
                }
            };
            let count = match key {
                "bbox" | "extent" => 4,
                "size" | "resolution" => 2,
                _ => 1,
            };
            while parts(&value).len() < count && i < tokens.len() && (!tokens[i].starts_with('-') || tokens[i].parse::<f64>().is_ok()) {
                value.push(',');
                value += &tokens[i];
                i += 1;
            }
            out.values.entry(key.into()).or_default().push(value);
        }
        Ok(out)
    }
    fn one(&mut self, key: &str) -> Result<Option<String>> {
        let Some(mut v) = self.values.remove(key) else { return Ok(None) };
        if v.len() != 1 {
            return Err(bad(&format!("--{key} cannot be repeated")));
        }
        Ok(v.pop())
    }
    fn many(&mut self, key: &str) -> Vec<String> {
        self.values.remove(key).unwrap_or_default()
    }
    fn flag(&mut self, key: &str) -> Result<bool> {
        self.one(key)?.map(|v| v.parse().map_err(|_| bad(&format!("invalid --{key} boolean")))).transpose().map(|v| v.unwrap_or(false))
    }
    fn required_pos(&mut self, key: &str) -> Result<String> {
        if let Some(v) = self.one(key)? {
            return Ok(v);
        }
        if !self.pos.is_empty() {
            return Ok(self.pos.remove(0));
        }
        Err(bad(&format!("missing {key}")))
    }
    fn array<const N: usize>(&mut self, key: &str) -> Result<Option<[f64; N]>> {
        self.one(key)?
            .map(|v| {
                let v =
                    parts(&v).iter().map(|s| s.parse::<f64>().map_err(|_| bad(&format!("invalid --{key}")))).collect::<Result<Vec<_>>>()?;
                if v.iter().any(|v| !v.is_finite()) {
                    return Err(bad(&format!("--{key} must be finite")));
                }
                v.try_into().map_err(|_| bad(&format!("--{key} needs {N} values")))
            })
            .transpose()
    }
    fn pair(&mut self, key: &str) -> Result<Option<(f64, f64)>> {
        Ok(self.array::<2>(key)?.map(|v| (v[0], v[1])))
    }
    fn usize_pair(&mut self, key: &str) -> Result<Option<(usize, usize)>> {
        self.one(key)?
            .map(|v| {
                let v = parts(&v);
                if v.len() != 2 {
                    return Err(bad(&format!("--{key} needs two values")));
                }
                let x = v[0].parse().map_err(|_| bad("invalid size"))?;
                let y = v[1].parse().map_err(|_| bad("invalid size"))?;
                if x == 0 || y == 0 {
                    return Err(bad("size must be positive"));
                }
                Ok((x, y))
            })
            .transpose()
    }
    fn positive(&mut self, key: &str, zero: bool) -> Result<f64> {
        let v = self.required_pos(key)?.parse::<f64>().map_err(|_| bad(&format!("invalid {key}")))?;
        if !v.is_finite() || v < 0. || (!zero && v == 0.) {
            return Err(bad(&format!("invalid {key}")));
        }
        Ok(v)
    }
    fn write(&mut self) -> Result<WriteSpec> {
        let path = self.required_pos("output")?;
        let format = self.one("format")?;
        let overwrite = self.flag("overwrite")?;
        let append = self.flag("append")?;
        let layer = self.one("output-layer")?;
        if overwrite && append {
            return Err(bad("append and overwrite are mutually exclusive"));
        }
        let spec = WriteSpec { path, format, overwrite, append, layer };
        if spec.definition() && spec.append {
            return Err(bad("saved pipeline definitions cannot append"));
        }
        Ok(spec)
    }
    fn finish(&self) -> Result<()> {
        if let Some(k) = self.values.keys().next() {
            return Err(bad(&format!("unsupported option --{k}")));
        }
        if !self.pos.is_empty() {
            return Err(bad("unexpected positional arguments"));
        }
        Ok(())
    }
}
