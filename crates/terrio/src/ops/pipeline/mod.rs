//! Linear raster/vector pipelines, with no serialized intermediate datasets.
//! Reference syntax and behavior: GDAL 3.13.3 pipelines and GDALG.
//!
//! The parser accepts the supported GDAL-style steps separated by `!` and saved
//! GDALG JSON. It never invokes a shell. Evaluation retains lazy raster sources;
//! only explicit materialization or operations requiring pixels load their result.
mod parse;
mod raster;
use crate::formats::{RasterDriver, VectorDriver};
use crate::ops::{
    vector_filter::{VectorTranslateOptions, WriteMode},
    warp::WarpOptions,
};
use crate::{Dataset, Error, Layer, LazyRaster, Result, VectorDataset, Window};
use parse::{Block, Step, WriteSpec};
use std::collections::BTreeMap;

/// Input dataset family. Auto uses the source format's normal detection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum InputKind {
    /// Detect raster or vector.
    #[default]
    Auto,
    /// Require a raster.
    Raster,
    /// Require vector layers.
    Vector,
}
impl std::str::FromStr for InputKind {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "auto" => Ok(Self::Auto),
            "raster" => Ok(Self::Raster),
            "vector" => Ok(Self::Vector),
            _ => Err(invalid("input kind must be auto, raster or vector")),
        }
    }
}
/// A lazy raster or materialized vector layers flowing through a pipeline.
#[allow(clippy::large_enum_variant)]
pub enum PipelineData {
    /// Window- and overview-readable raster data.
    Raster(LazyRaster),
    /// Vector layers, with schemas and CRS retained by their respective operations.
    Vector(Vec<Layer>),
}
impl From<Dataset> for PipelineData {
    fn from(ds: Dataset) -> Self {
        match ds {
            Dataset::Raster(r) => Self::Raster(LazyRaster::from(r)),
            Dataset::Vector(v) => Self::Vector(v.layers),
        }
    }
}
impl PipelineData {
    /// A read-only handle for another consumer. Raster handles share the source; vector layers are cloned.
    pub fn shared(&mut self) -> Self {
        match self {
            Self::Raster(r) => Self::Raster(r.shared()),
            Self::Vector(l) => Self::Vector(l.clone()),
        }
    }
    /// The dataset family.
    pub fn kind(&self) -> InputKind {
        match self {
            Self::Raster(_) => InputKind::Raster,
            Self::Vector(_) => InputKind::Vector,
        }
    }
    /// Loads remaining lazy pixels and their dataset mask into memory.
    pub fn materialize(&mut self) -> Result<()> {
        if let Self::Raster(r) = self {
            let referenced = r.geotransform_present();
            let ds = r.read()?;
            let mask = r.read_mask(Window::full(r.width(), r.height()), None)?;
            let snapshot = if let Some(mask) = mask { LazyRaster::from_masked_dataset(ds, mask)? } else { LazyRaster::from(ds) };
            *r = raster::preserve_reference(snapshot, referenced)?;
        }
        Ok(())
    }
    /// Converts the final value to the ordinary eager Dataset API.
    /// Raster masks remain available through `PipelineData::Raster` before this conversion.
    pub fn into_dataset(self) -> Result<Dataset> {
        match self {
            Self::Raster(mut r) => Ok(Dataset::Raster(r.read()?)),
            Self::Vector(layers) => Ok(Dataset::Vector(VectorDataset { layers, driver: VectorDriver::GeoJSON })),
        }
    }
    fn vectors(&self) -> Result<&[Layer]> {
        match self {
            Self::Vector(l) => Ok(l),
            _ => Err(invalid("step requires vector input")),
        }
    }
    fn raster(&mut self) -> Result<&mut LazyRaster> {
        match self {
            Self::Raster(r) => Ok(r),
            _ => Err(invalid("step requires raster input")),
        }
    }
    fn rename(&mut self, name: &Option<String>) -> Result<()> {
        if let Some(name) = name {
            let Self::Vector(layers) = self else { return Err(invalid("output-layer requires vector input")) };
            if layers.len() != 1 {
                return Err(invalid("output-layer requires one selected layer"));
            }
            layers[0].name = name.clone();
        }
        Ok(())
    }
    fn snapshot_if_overwritten(&mut self, paths: &[String]) -> Result<()> {
        if let Self::Raster(r) = self {
            match r.input_paths() {
                Ok(sources) if !paths.iter().any(|p| crate::ops::strips::overwrites(p, &sources)) => {}
                _ => self.materialize()?,
            }
        }
        Ok(())
    }
}
/// A request to the caller's input loader. Aliases are kept separate from resolved paths.
#[derive(Debug, Clone)]
pub struct ReadRequest {
    /// Source token as supplied in the pipeline, for uploaded or in-memory inputs.
    pub source: String,
    /// Source path resolved against the saved definition's base, when applicable.
    pub resolved: String,
    /// Explicit or auto-detected input family.
    pub kind: InputKind,
}
impl ReadRequest {
    /// Finds the caller's input for this request: by source token, then by resolved path.
    pub fn find_input<'a, V>(&self, inputs: &'a BTreeMap<String, V>) -> Option<&'a V> {
        inputs.get(&self.source).or_else(|| inputs.iter().find(|(name, _)| same_path(name, &self.resolved)).map(|(_, value)| value))
    }
}
/// Resolved paths always join with `/`, so on Windows a native key names the same file.
fn same_path(a: &str, b: &str) -> bool {
    same_path_on(a, b, cfg!(windows))
}
fn same_path_on(a: &str, b: &str, windows: bool) -> bool {
    if windows { windows_segments(a) == windows_segments(b) } else { a == b }
}
/// A Windows path's segments, with `.` and `..` applied, since a resolved path can retain them.
fn windows_segments(path: &str) -> Vec<&str> {
    let mut segments = Vec::new();
    for segment in path.split(['\\', '/']) {
        match segment {
            "." => {}
            ".." if segments.last().is_some_and(|last: &&str| !last.is_empty() && !last.ends_with(':') && *last != "..") => {
                segments.pop();
            }
            _ => segments.push(segment),
        }
    }
    segments
}
/// A loaded input dataset. Nested checkpoint and write artifacts are discarded.
pub struct PipelineInput {
    data: PipelineData,
}
impl PipelineInput {
    /// Loads a native path or a saved definition's evaluated dataset.
    /// Nested checkpoints and writes are discarded.
    pub fn open(request: &ReadRequest) -> Result<Self> {
        load_path(request, 0)
    }
}
impl From<PipelineData> for PipelineInput {
    fn from(data: PipelineData) -> Self {
        Self { data }
    }
}
/// A validated reusable pipeline. Input files are not opened while parsing.
#[derive(Debug, Clone)]
pub struct Pipeline {
    kind: InputKind,
    blocks: Vec<Block>,
    base: Option<String>,
    relative: bool,
}
impl Pipeline {
    /// Parses a command string with POSIX quoting; no shell expansion is performed.
    /// Outside quotes a backslash escapes the next character, so pass Windows paths in
    /// single quotes (`'C:\data\in.tif'`) or with forward slashes.
    pub fn parse(command: &str) -> Result<Self> {
        let tokens = shlex::split(command).ok_or_else(|| invalid("unterminated command quoting"))?;
        Self::from_tokens(&tokens)
    }
    /// Parses already-tokenized arguments, such as a CLI's trailing arguments.
    pub fn from_tokens(tokens: &[String]) -> Result<Self> {
        if tokens.iter().any(|s| s.contains('\0')) {
            return Err(invalid("NUL is not allowed in pipeline arguments"));
        }
        let (kind, blocks) = parse::parse(tokens)?;
        Ok(Self { kind, blocks, base: None, relative: false })
    }
    /// The requested dataset family, including a raster/vector command prefix.
    pub fn input_kind(&self) -> InputKind {
        self.kind
    }
    /// Overrides input detection without opening the input.
    pub fn with_kind(mut self, kind: InputKind) -> Self {
        self.kind = kind;
        self
    }
    /// Replaces or appends the terminal write destination.
    pub fn with_output(mut self, path: &str, format: Option<&str>, overwrite: bool) -> Self {
        let layer = match self.blocks.last() {
            Some(Block { step: Step::Write(spec), .. }) => spec.layer.clone(),
            _ => None,
        };
        if self.blocks.last().is_some_and(|b| matches!(b.step, Step::Write(_))) {
            self.blocks.pop();
        }
        self.blocks.push(Block {
            argv: vec!["write".into(), path.into()],
            step: Step::Write(WriteSpec { path: path.into(), format: format.map(str::to_string), overwrite, append: false, layer }),
        });
        self
    }
    /// Allows replacement of the terminal output, requiring an explicit write step.
    pub fn overwrite_output(mut self) -> Result<Self> {
        let Some(Block { step: Step::Write(spec), .. }) = self.blocks.last_mut() else {
            return Err(invalid("--overwrite requires a final write"));
        };
        spec.overwrite = true;
        spec.append = false;
        Ok(self)
    }
    /// Parses a GDALG definition. Relative paths use `base_file` when the schema requests it.
    pub fn from_gdalg(text: &str, base_file: Option<&str>) -> Result<Self> {
        let doc: serde_json::Value = serde_json::from_str(text).map_err(|e| invalid(&format!("invalid GDALG JSON: {e}")))?;
        if doc.get("type").and_then(|v| v.as_str()) != Some("gdal_streamed_alg") {
            return Err(invalid("GDALG type must be gdal_streamed_alg"));
        }
        let command = doc.get("command_line").and_then(|v| v.as_str()).ok_or_else(|| invalid("GDALG needs command_line"))?;
        let mut plan = Self::parse(command)?;
        plan.relative = match doc.get("relative_paths_relative_to_this_file") {
            None => true,
            Some(v) => v.as_bool().ok_or_else(|| invalid("relative_paths_relative_to_this_file must be boolean"))?,
        };
        plan.base = base_file.map(str::to_string);
        Ok(plan)
    }
    /// Loads a local, VSI or remote saved definition, without reading its input pixels.
    pub fn open(path: &str) -> Result<Self> {
        let bytes = crate::vsi::read(path)?;
        let text = std::str::from_utf8(&bytes).map_err(|_| invalid("GDALG must be UTF-8"))?;
        Self::from_gdalg(text, Some(path))
    }
    /// Serializes a GDAL-readable definition, omitting a final write step.
    /// With no saved-file base, relative tokens retain current-directory semantics.
    pub fn to_gdalg(&self) -> Result<String> {
        let mixed = self.blocks.iter().any(|b| matches!(b.step, Step::AsFeatures(_) | Step::Rasterize(_)));
        let mut tokens = vec!["gdal".to_string()];
        if !mixed {
            match self.kind {
                InputKind::Raster => tokens.push("raster".into()),
                InputKind::Vector => tokens.push("vector".into()),
                _ => {}
            }
        }
        tokens.push("pipeline".into());
        let prefix = tokens.join(" ");
        // GDALG uses GDAL's command tokenizer, which accepts double quotes, not
        // POSIX single quotes. Delimiters must remain outside argument quotes.
        let quote = |s: &str| format!("\"{}\"", s.replace('\\', "\\\\").replace('\"', "\\\""));
        let steps = self
            .blocks
            .iter()
            .filter(|b| !matches!(b.step, Step::Write(_)))
            .map(|b| b.argv.iter().map(|arg| quote(arg)).collect::<Vec<_>>().join(" "))
            .collect::<Vec<_>>();
        let command = format!("{prefix} {}", steps.join(" ! "));
        serde_json::to_string_pretty(
            &serde_json::json!({"type":"gdal_streamed_alg","command_line":command,"relative_paths_relative_to_this_file":self.relative}),
        )
        .map_err(|e| invalid(&e.to_string()))
    }
    /// Evaluates with caller-provided inputs, keeping all outputs in memory until requested.
    /// Named materialize checkpoints are snapshots; no implicit temporary files are created.
    pub fn evaluate_with(&self, mut load: impl FnMut(&ReadRequest) -> Result<PipelineInput>) -> Result<PipelineResult> {
        if let Some(Block { step: Step::Write(spec), .. }) = self.blocks.last()
            && spec.definition()
        {
            return Ok(PipelineResult {
                data: None,
                artifacts: vec![Artifact::Definition { spec: spec.clone(), bytes: self.to_gdalg()?.into_bytes() }],
            });
        }
        let mut data = None;
        let mut artifacts = vec![];
        for block in &self.blocks {
            if let Step::Read { source, layers } = &block.step {
                let mut loaded = load(&self.request(source))?;
                if self.kind != InputKind::Auto && loaded.data.kind() != self.kind {
                    return Err(invalid("read input kind does not match the requested family"));
                }
                if !layers.is_empty() {
                    let PipelineData::Vector(input) = &mut loaded.data else {
                        return Err(invalid("read layer selection requires vector input"));
                    };
                    for name in layers {
                        if !input.iter().any(|l| &l.name == name) {
                            return Err(invalid(&format!("input layer {name:?} not found")));
                        }
                    }
                    input.retain(|l| layers.contains(&l.name));
                }
                data = Some(loaded.data);
                continue;
            }
            let current = data.as_mut().ok_or_else(|| invalid("missing read input"))?;
            match &block.step {
                Step::Write(spec) => {
                    let mut output = current.shared();
                    output.rename(&spec.layer)?;
                    artifacts.push(Artifact::Data { spec: spec.clone(), data: output });
                }
                Step::Materialize(spec) => {
                    current.materialize()?;
                    if let Some(spec) = spec {
                        let mut checkpoint = current.shared();
                        checkpoint.rename(&spec.layer)?;
                        artifacts.push(Artifact::Data { spec: spec.clone(), data: checkpoint });
                    }
                }
                step => apply(current, step)?,
            }
        }
        Ok(PipelineResult { data, artifacts })
    }
    /// Evaluates native paths and explicitly writes the final output/checkpoints.
    /// Every destination is checked before any write; input/output aliases are snapshotted.
    pub fn run(&self) -> Result<PipelineResult> {
        let mut plan = self.clone();
        // Native exports resolve input paths before saving so definitions remain
        // usable when moved or opened from another working directory.
        let cwd = std::env::current_dir()?.join("__pipeline__.gdalg.json").to_string_lossy().into_owned();
        for i in 0..plan.blocks.len() {
            if let Step::Read { source, layers } = &self.blocks[i].step {
                let request = self.request(source);
                let resolved = if self.base.is_some() && self.relative {
                    request.resolved
                } else {
                    crate::ops::vrt::resolve_reference(&cwd, source, true)
                };
                plan.blocks[i] = Block {
                    argv: std::iter::once("read".into())
                        .chain(std::iter::once(format!("--input={resolved}")))
                        .chain(layers.iter().flat_map(|l| ["--layer".into(), l.clone()]))
                        .collect(),
                    step: Step::Read { source: resolved, layers: layers.clone() },
                };
            }
        }
        plan.base = None;
        plan.relative = false;
        let mut result = plan.evaluate_with(|r| load_path(r, 0))?;
        result.write_outputs()?;
        Ok(result)
    }
    fn request(&self, source: &str) -> ReadRequest {
        ReadRequest {
            source: source.into(),
            resolved: if self.relative {
                self.base.as_ref().map_or_else(|| source.to_string(), |base| crate::ops::vrt::resolve_reference(base, source, true))
            } else {
                source.into()
            },
            kind: self.kind,
        }
    }
}

enum Artifact {
    Data { spec: WriteSpec, data: PipelineData },
    Definition { spec: WriteSpec, bytes: Vec<u8> },
}
impl Artifact {
    fn spec(&self) -> &WriteSpec {
        match self {
            Self::Data { spec, .. } | Self::Definition { spec, .. } => spec,
        }
    }
}
/// The computed final dataset and deferred, explicitly named outputs.
pub struct PipelineResult {
    /// None only when the pipeline saves a GDALG definition without evaluating it.
    pub data: Option<PipelineData>,
    artifacts: Vec<Artifact>,
}
impl PipelineResult {
    /// The final output and checkpoint names in execution order.
    pub fn output_names(&self) -> Vec<String> {
        self.artifacts.iter().map(|a| a.spec().path.clone()).collect()
    }
    /// Encodes requested outputs to bytes, suitable for browser downloads or uploaded inputs.
    pub fn output_bytes(&mut self) -> Result<BTreeMap<String, Vec<u8>>> {
        if self.artifacts.iter().any(|a| a.spec().append) {
            return Err(invalid("byte outputs cannot append; use native write_outputs"));
        }
        let mut outputs = BTreeMap::new();
        for a in &mut self.artifacts {
            let bytes = match a {
                Artifact::Definition { bytes, .. } => bytes.clone(),
                Artifact::Data { spec, data } => match data {
                    PipelineData::Raster(r) => {
                        let mut options = raster_options(spec)?;
                        keep_unreferenced(r, &mut options, &spec.path);
                        r.prepare_output_mask(&spec.path, &mut options, Window::full(r.width(), r.height()), None)?;
                        crate::memory::write_raster_bytes(&r.read()?, &spec.path, &options)?
                    }
                    PipelineData::Vector(layers) => crate::memory::write_vector_bytes(layers, &spec.path, &vector_options(spec)?)?,
                },
            };
            if outputs.insert(a.spec().path.clone(), bytes).is_some() {
                return Err(invalid("duplicate output names cannot be encoded as a map"));
            }
        }
        Ok(outputs)
    }
    /// Writes every requested output after checking formats, collisions and input aliases.
    pub fn write_outputs(&mut self) -> Result<()> {
        let paths = self.output_names();
        for (i, a) in self.artifacts.iter().enumerate() {
            let s = a.spec();
            if !s.overwrite && !s.append && (std::fs::symlink_metadata(&s.path).is_ok() || crate::vsi::stat(&s.path)?.is_some()) {
                return Err(invalid(&format!("output {:?} exists; use --overwrite or --append", s.path)));
            }
            if !s.overwrite
                && !s.append
                && paths[..i].iter().any(|p| p == &s.path || crate::ops::strips::overwrites(&s.path, std::slice::from_ref(p)))
            {
                return Err(invalid("duplicate output needs explicit overwrite or append"));
            }
            match a {
                Artifact::Definition { .. } => {}
                Artifact::Data { spec, data } => match data {
                    PipelineData::Raster(_) => {
                        if spec.append {
                            return Err(invalid("raster pipeline writes do not support append"));
                        }
                        raster_options(spec)?;
                    }
                    PipelineData::Vector(_) => {
                        vector_options(spec)?;
                    }
                },
            }
        }
        if let Some(data) = &mut self.data {
            data.snapshot_if_overwritten(&paths)?;
        }
        for a in &mut self.artifacts {
            if let Artifact::Data { data, .. } = a {
                data.snapshot_if_overwritten(&paths)?;
            }
        }
        for a in &mut self.artifacts {
            match a {
                Artifact::Definition { spec, bytes } => crate::vsi::write(&spec.path, bytes)?,
                Artifact::Data { spec, data } => match data {
                    PipelineData::Raster(r) => {
                        let mut options = raster_options(spec)?;
                        keep_unreferenced(r, &mut options, &spec.path);
                        r.prepare_output_mask(&spec.path, &mut options, Window::full(r.width(), r.height()), None)?;
                        if options.mask.is_some() && options.driver == Some(RasterDriver::Cog) {
                            crate::write_raster_dataset(&r.read()?, &spec.path, &options)?;
                        } else {
                            r.write(&spec.path, &options)?;
                        }
                    }
                    PipelineData::Vector(layers) => crate::ops::vector_filter::write_vector_with_mode(
                        layers,
                        &spec.path,
                        &vector_options(spec)?,
                        if spec.append {
                            WriteMode::Append
                        } else if spec.overwrite {
                            WriteMode::Overwrite
                        } else {
                            WriteMode::Create
                        },
                    )?,
                },
            }
        }
        Ok(())
    }
}
/// Keeps an unreferenced GeoTIFF/COG output unreferenced. Applied only to pipeline outputs, so
/// other writers that share `prepare_output_mask` (such as `palette`) keep their own output.
fn keep_unreferenced(raster: &LazyRaster, options: &mut crate::RasterWriteOptions, path: &str) {
    let driver = options.driver.or_else(|| RasterDriver::for_extension(path));
    if matches!(driver, Some(RasterDriver::GTiff | RasterDriver::Cog))
        && !raster.geotransform_present()
        && options.geo_transform.is_none()
        && !options.creation_options.iter().any(|(key, _)| key.eq_ignore_ascii_case("GEOREFERENCED"))
    {
        options.creation_options.push(("GEOREFERENCED".into(), "NO".into()));
    }
}
fn raster_options(spec: &WriteSpec) -> Result<crate::RasterWriteOptions> {
    let driver =
        spec.format.as_ref().map(|s| RasterDriver::from_name(s).ok_or_else(|| invalid("unsupported raster output format"))).transpose()?;
    if let Some(driver) = driver.or_else(|| RasterDriver::for_extension(&spec.path)) {
        driver.ensure_built()?;
    } else {
        return Err(invalid("raster output needs a recognized extension or format"));
    }
    Ok(crate::RasterWriteOptions { driver, ..Default::default() })
}
fn vector_options(spec: &WriteSpec) -> Result<crate::VectorWriteOptions> {
    let driver =
        spec.format.as_ref().map(|s| VectorDriver::from_name(s).ok_or_else(|| invalid("unsupported vector output format"))).transpose()?;
    if let Some(driver) = driver.or_else(|| VectorDriver::for_extension(&spec.path)) {
        driver.ensure_built()?;
    } else {
        return Err(invalid("vector output needs a recognized extension or format"));
    }
    Ok(crate::VectorWriteOptions { driver, ..Default::default() })
}
fn load_path(request: &ReadRequest, depth: usize) -> Result<PipelineInput> {
    if depth >= 16 {
        return Err(invalid("cyclic or excessively nested saved pipelines"));
    }
    let path = &request.resolved;
    if path.to_ascii_lowercase().ends_with(".gdalg.json") {
        let plan = Pipeline::open(path)?;
        let mut result = plan.evaluate_with(|r| load_path(r, depth + 1))?;
        let data = result.data.take().ok_or_else(|| invalid("saved input pipeline has no dataset"))?;
        return Ok(data.into());
    }
    let data = match request.kind {
        InputKind::Raster => PipelineData::Raster(crate::open_raster_lazy(path)?),
        InputKind::Vector => PipelineData::Vector(crate::open_vector(path, &Default::default())?.layers),
        InputKind::Auto => {
            if RasterDriver::for_extension(path).is_some() {
                match crate::open_raster_lazy(path) {
                    Ok(r) => PipelineData::Raster(r),
                    Err(raster_error) => match crate::open_vector(path, &Default::default()) {
                        Ok(v) => PipelineData::Vector(v.layers),
                        Err(_) => return Err(raster_error),
                    },
                }
            } else {
                match crate::open_vector(path, &Default::default()) {
                    Ok(ds) => PipelineData::Vector(ds.layers),
                    Err(_) => PipelineData::Raster(crate::open_raster_lazy(path)?),
                }
            }
        }
    };
    Ok(data.into())
}
fn apply(data: &mut PipelineData, step: &Step) -> Result<()> {
    use crate::ops::{
        topology,
        vector_geom::{self, GeometryOp},
    };
    match step {
        Step::Filter(options) => {
            *data = PipelineData::Vector(crate::ops::vector_filter::translate(data.vectors()?, options)?);
        }
        Step::Select(values) => match data {
            PipelineData::Vector(layers) => {
                *layers = crate::ops::vector_filter::translate(
                    layers,
                    &VectorTranslateOptions {
                        select: Some(values.iter().filter(|s| s.as_str() != "_ogr_geometry_").cloned().collect()),
                        ..Default::default()
                    },
                )?;
                if !values.iter().any(|s| s == "_ogr_geometry_") {
                    for layer in layers {
                        for feature in &mut layer.features {
                            feature.geometry = None;
                        }
                        layer.geom_type = None;
                        layer.set_crs_epsg(None);
                        layer.set_crs_wkt(None);
                    }
                }
            }
            PipelineData::Raster(r) => {
                let bands = values
                    .iter()
                    .map(|s| s.parse::<usize>().map_err(|_| invalid("raster select needs numeric band indexes")))
                    .collect::<Result<Vec<_>>>()?;
                let source = r.shared();
                *r = raster::view(source, bands, None)?;
            }
        },
        Step::Clip { bbox } => match data {
            PipelineData::Vector(layers) => {
                let region = crate::ops::vector_clip::ClipRegion::bbox(*bbox, None)?;
                *layers = layers.iter().map(|l| crate::ops::vector_clip::clip(l, &region)).collect::<Result<_>>()?;
            }
            PipelineData::Raster(r) => {
                let gt = r.header().geo_transform();
                if gt[2] != 0. || gt[4] != 0. || gt[1] <= 0. || gt[5] >= 0. {
                    return Err(invalid("raster bbox clip requires a north-up grid; reproject first"));
                }
                let x0 = ((bbox[0] - gt[0]) / gt[1] + 0.001).floor();
                let y0 = ((bbox[3] - gt[3]) / gt[5] + 0.001).floor();
                let x1 = ((bbox[2] - gt[0]) / gt[1] - 0.001).ceil();
                let y1 = ((bbox[1] - gt[3]) / gt[5] - 0.001).ceil();
                if x0 < 0. || y0 < 0. || x1 > r.width() as f64 || y1 > r.height() as f64 {
                    return Err(invalid("pipeline raster clip bbox must lie within the source"));
                }
                let crop = Window::new(x0 as usize, y0 as usize, (x1 - x0) as usize, (y1 - y0) as usize);
                let source = r.shared();
                *r = raster::view(source, vec![], Some(crop))?;
            }
        },
        Step::Reproject { crs, resampling, size, resolution } => match data {
            PipelineData::Vector(layers) => {
                if size.is_some() || resolution.is_some() {
                    return Err(invalid("vector reproject does not accept a raster grid"));
                }
                if resampling.is_some() {
                    return Err(invalid("vector reproject does not accept --resampling"));
                }
                *layers = crate::ops::vector_filter::translate(
                    layers,
                    &VectorTranslateOptions { dst_srs: Some(crs.clone()), ..Default::default() },
                )?;
            }
            PipelineData::Raster(r) => {
                *r = raster::warp(
                    r,
                    &WarpOptions {
                        dst_srs: Some(crs.clone()),
                        resampling: resampling.unwrap_or_default(),
                        size: *size,
                        resolution: *resolution,
                        ..Default::default()
                    },
                )?;
            }
        },
        Step::Resize { size, resolution, resampling } => {
            let r = data.raster()?;
            let h = r.header();
            let gt = h.geo_transform();
            if gt[2] != 0. || gt[4] != 0. || gt[1] <= 0. || gt[5] >= 0. {
                return Err(invalid("resize requires a north-up grid; reproject first"));
            }
            let size = size.as_ref().map(|(x, y)| Ok::<_, Error>((pixels(x, r.width())?, pixels(y, r.height())?))).transpose()?;
            let bbox = (gt[0], gt[3] + r.height() as f64 * gt[5], gt[0] + r.width() as f64 * gt[1], gt[3]);
            *r = raster::resize(
                r,
                &WarpOptions { extent: Some(bbox), size, resolution: *resolution, resampling: *resampling, ..Default::default() },
            )?;
        }
        Step::SetType(dtype) => {
            let r = data.raster()?;
            if *dtype != r.data_type() {
                let referenced = r.geotransform_present();
                let mask = r.read_mask(Window::full(r.width(), r.height()), None)?;
                let ds = r.read()?;
                let mut converted = crate::ops::translate::translate(
                    &ds,
                    &crate::ops::translate::TranslateOptions { output_type: Some(*dtype), ..Default::default() },
                )?;
                raster::clear_statistics(&mut converted);
                let converted =
                    if let Some(mask) = mask { LazyRaster::from_masked_dataset(converted, mask)? } else { LazyRaster::from(converted) };
                *r = raster::preserve_reference(converted, referenced)?;
            }
        }
        Step::Buffer { distance, quadrant_segments } => {
            *data = PipelineData::Vector(
                data.vectors()?
                    .iter()
                    .map(|l| topology::buffer(l, &topology::BufferOptions { distance: *distance, quadrant_segments: *quadrant_segments }))
                    .collect::<Result<_>>()?,
            );
        }
        Step::Simplify(tolerance) => {
            *data = PipelineData::Vector(
                data.vectors()?
                    .iter()
                    .map(|l| vector_geom::apply(l, &GeometryOp::Simplify { tolerance: *tolerance }))
                    .collect::<Result<_>>()?,
            );
        }
        Step::Segmentize(max_length) => {
            *data = PipelineData::Vector(
                data.vectors()?
                    .iter()
                    .map(|l| vector_geom::apply(l, &GeometryOp::Segmentize { max_length: *max_length }))
                    .collect::<Result<_>>()?,
            );
        }
        Step::SwapXY => {
            *data =
                PipelineData::Vector(data.vectors()?.iter().map(|l| vector_geom::apply(l, &GeometryOp::SwapXy)).collect::<Result<_>>()?);
        }
        Step::Dissolve => {
            *data = PipelineData::Vector(data.vectors()?.iter().map(super::layer_algebra::dissolve_features).collect::<Result<_>>()?);
        }
        Step::AsFeatures(options) => {
            let ds = data.raster()?.read()?;
            *data = PipelineData::Vector(vec![crate::ops::as_features::as_features(&ds, options)?]);
        }
        Step::Rasterize(options) => {
            *data = PipelineData::Raster(LazyRaster::from(crate::ops::rasterize::rasterize(data.vectors()?, options)?));
        }
        _ => return Err(invalid("unexpected pipeline control step")),
    }
    Ok(())
}
fn pixels(s: &str, source: usize) -> Result<usize> {
    let size = match crate::ops::translate::Size::parse(s)? {
        crate::ops::translate::Size::Pixels(v) => v,
        crate::ops::translate::Size::Percent(v) => {
            if !v.is_finite() || v <= 0. {
                return Err(invalid("resize percentage must be positive"));
            }
            (source as f64 * v / 100. + 0.5) as usize
        }
    };
    if size == 0 {
        return Err(invalid("resize dimensions must be positive"));
    }
    Ok(size)
}
fn invalid(s: &str) -> Error {
    Error::InvalidArgument(format!("pipeline: {s}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Feature, FieldDef, FieldType, FieldValue, Geometry};
    use wbvector::Coord;
    fn vectors() -> PipelineData {
        let mut layer = Layer::new("places").with_epsg(3857);
        layer.schema.add_field(FieldDef::new("name", FieldType::Text));
        layer.schema.add_field(FieldDef::new("score", FieldType::Integer));
        for (i, score) in [1, 3, 5].into_iter().enumerate() {
            layer.features.push(Feature {
                fid: i as u64 + 10,
                geometry: Some(Geometry::Point(Coord::xy(i as f64 * 10., 0.))),
                attributes: vec![FieldValue::Text(format!("place-{i}")), FieldValue::Integer(score)],
            });
        }
        PipelineData::Vector(vec![layer])
    }
    fn evaluate(plan: &str) -> PipelineResult {
        Pipeline::parse(plan).unwrap().evaluate_with(|_| Ok(vectors().into())).unwrap()
    }
    #[test]
    fn vector_chain_retains_fields_crs_and_composes_without_files() {
        let mut result =
            evaluate("read alias ! filter --where 'score >= 3' ! select name,_ogr_geometry_ ! materialize ! buffer 1 ! write out.geojson");
        let data = result.data.as_ref().unwrap().vectors().unwrap();
        assert_eq!(data[0].features.len(), 2);
        assert_eq!(data[0].schema.len(), 1);
        assert_eq!(data[0].schema.fields()[0].name, "name");
        assert!(crate::crs::SpatialRef::from_layer(&data[0]).same_as(&crate::crs::SpatialRef::parse("EPSG:3857").unwrap()));
        assert!(matches!(data[0].features[0].geometry, Some(Geometry::MultiPolygon(_))));
        assert_eq!(result.output_names(), vec!["out.geojson"]);
        let bytes = result.output_bytes().unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes["out.geojson"]).unwrap();
        assert_eq!(json["features"].as_array().unwrap().len(), 2);
    }
    #[test]
    fn checkpoint_layer_names_apply_only_to_the_snapshot() {
        let result = evaluate("read alias ! materialize --output checkpoint.geojson --output-layer checkpoint ! write final.geojson");
        let Artifact::Data { data, .. } = &result.artifacts[0] else { panic!("checkpoint") };
        assert_eq!(data.vectors().unwrap()[0].name, "checkpoint");
        assert_eq!(result.artifacts[1].spec().layer, None);
        assert_eq!(result.data.as_ref().unwrap().vectors().unwrap()[0].name, "places");
    }
    #[test]
    fn final_output_layer_does_not_rename_the_returned_dataset_and_survives_override() {
        let plan =
            Pipeline::parse("read alias ! write old.geojson --output-layer renamed").unwrap().with_output("new.geojson", None, false);
        let mut result = plan.evaluate_with(|_| Ok(vectors().into())).unwrap();
        assert_eq!(result.data.as_ref().unwrap().vectors().unwrap()[0].name, "places");
        let Artifact::Data { data, spec } = &result.artifacts[0] else { panic!("output") };
        assert_eq!(data.vectors().unwrap()[0].name, "renamed");
        assert_eq!(spec.path, "new.geojson");
        assert!(result.output_bytes().unwrap().contains_key("new.geojson"));
    }
    #[test]
    fn missing_multivalue_option_does_not_consume_the_next_step() {
        let error = Pipeline::parse("read missing ! resize --resolution 10 ! write out.tif").unwrap_err();
        assert!(error.to_string().contains("needs 2 values"), "{error}");
    }
    #[test]
    fn missing_multivalue_options_do_not_consume_following_options() {
        let error = Pipeline::parse("read alias ! resize --resolution 10 --resampling near").unwrap_err();
        assert!(error.to_string().contains("needs 2 values"), "{error}");
        let plan = Pipeline::parse("read alias ! filter --where '' --bbox -1 -2 3 4").unwrap();
        let Step::Filter(options) = &plan.blocks[1].step else { panic!("filter") };
        assert_eq!(options.where_clause.as_deref(), Some(""));
        assert_eq!(options.spatial_filter, Some((-1., -2., 3., 4.)));
    }
    #[test]
    fn vector_reproject_rejects_raster_resampling() {
        let plan = Pipeline::parse("read alias ! reproject --output-crs EPSG:4326 --resampling bilinear").unwrap();
        assert!(plan.evaluate_with(|_| Ok(vectors().into())).is_err());
    }
    #[test]
    fn command_quoting_keeps_single_quoted_windows_paths_literal() {
        let plan = Pipeline::parse(r"read 'C:\data\in.tif' ! write out.tif").unwrap();
        assert!(matches!(&plan.blocks[0].step, Step::Read { source, .. } if source == r"C:\data\in.tif"));
        let unquoted = Pipeline::parse(r"read C:\data\in.tif").unwrap();
        assert!(matches!(&unquoted.blocks[0].step, Step::Read { source, .. } if source == r"C:datain.tif"));
    }
    #[test]
    fn dissolve_is_per_feature_and_unions_nodes_and_merges_components() {
        use crate::Geometry;
        use wbvector::Coord;
        let line = |points: &[(f64, f64)]| points.iter().map(|&(x, y)| Coord::xy(x, y)).collect();
        let mut data = vectors();
        let PipelineData::Vector(layers) = &mut data else { unreachable!() };
        layers[0].features[0].geometry = Some(Geometry::MultiLineString(vec![line(&[(0., 0.), (4., 0.)]), line(&[(2., 0.), (6., 0.)])]));
        layers[0].features[1].geometry = Some(Geometry::MultiLineString(vec![line(&[(0., 0.), (4., 4.)]), line(&[(0., 4.), (4., 0.)])]));
        layers[0].features[2].geometry = Some(Geometry::MultiPoint(vec![Coord::xy(1., 1.), Coord::xy(1., 1.), Coord::xy(2., 2.)]));
        let before = layers[0].clone();
        let result = Pipeline::parse("read alias ! dissolve").unwrap().evaluate_with(|_| Ok(data.shared().into())).unwrap();
        let result_data = result.data.unwrap();
        let after = &result_data.vectors().unwrap()[0];
        assert_eq!(after.features.len(), 3);
        assert_eq!(after.schema.len(), before.schema.len());
        for (actual, original) in after.features.iter().zip(&before.features) {
            assert_eq!(actual.fid, original.fid);
            assert_eq!(actual.attributes, original.attributes);
        }
        let Some(Geometry::LineString(line)) = &after.features[0].geometry else { panic!("overlapping lines must merge") };
        assert_eq!(line.len(), 4);
        let Some(Geometry::MultiLineString(lines)) = &after.features[1].geometry else { panic!("crossing lines must be noded") };
        assert_eq!(lines.len(), 4);
        assert!(lines.iter().all(|line| line.iter().any(|p| p.x == 2. && p.y == 2.)));
        assert!(matches!(&after.features[2].geometry,Some(Geometry::MultiPoint(points)) if points.len()==2));
    }
    #[test]
    fn saved_definitions_quote_paths_and_do_not_evaluate_sources() {
        let plan = Pipeline::parse("vector read 'a $() with spaces.geojson' ! filter --where \"name = 'place-0'\" ! write plan.gdalg.json")
            .unwrap();
        let text = plan.to_gdalg().unwrap();
        let doc: serde_json::Value = serde_json::from_str(&text).unwrap();
        let command = doc["command_line"].as_str().unwrap();
        assert!(command.contains(" ! \"filter\" "));
        assert!(!command.contains("'!'"));
        let roundtrip = Pipeline::from_gdalg(&text, None).unwrap();
        assert_eq!(roundtrip.blocks.len(), 2);
        let mut calls = 0;
        let mut result = plan
            .evaluate_with(|_| {
                calls += 1;
                Err(invalid("must not read while saving"))
            })
            .unwrap();
        assert_eq!(calls, 0);
        assert!(result.data.is_none());
        assert_eq!(result.output_bytes().unwrap()["plan.gdalg.json"], text.into_bytes());
    }
    #[test]
    fn invalid_steps_and_unknown_options_fail_before_loading() {
        for command in [
            "read a ! external sh -c test ! write b",
            "read a ! set-type Float32",
            "read a ! dissolve --group-by name",
            "read a ! write saved.gdalg.json --append",
            "read a ! filter --unknown 3",
            "read a ! write b ! buffer 1",
            "read a ! ! write b",
            "buffer 1 ! write b",
            "read a ! resize --size 2,2 --resolution 1,1",
        ] {
            assert!(Pipeline::parse(command).is_err(), "{command}");
        }
        assert!(Pipeline::from_gdalg(r#"{"type":"other","command_line":"read a"}"#, None).is_err());
    }
    #[test]
    fn preflight_all_outputs_before_creating_checkpoints() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("checkpoint.geojson");
        let final_path = dir.path().join("protected.geojson");
        std::fs::write(&final_path, b"preserve this").unwrap();
        let tokens = vec![
            "read".into(),
            "alias".into(),
            "!".into(),
            "materialize".into(),
            "--output".into(),
            first.to_string_lossy().into_owned(),
            "!".into(),
            "write".into(),
            final_path.to_string_lossy().into_owned(),
        ];
        let mut result = Pipeline::from_tokens(&tokens).unwrap().evaluate_with(|_| Ok(vectors().into())).unwrap();
        assert!(result.write_outputs().is_err());
        assert!(!first.exists());
        assert_eq!(std::fs::read(final_path).unwrap(), b"preserve this");
    }
    #[test]
    fn recursive_saved_pipelines_resolve_relative_inputs_and_guard_cycles() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        let input = dir.path().join("data.geojson");
        crate::write_vector(vectors().vectors().unwrap(), input.to_str().unwrap(), &Default::default()).unwrap();
        let path = nested.join("plan.gdalg.json");
        std::fs::write(
            &path,
            r#"{"type":"gdal_streamed_alg","command_line":"gdal vector pipeline read ../data.geojson ! filter --where 'score > 1'"}"#,
        )
        .unwrap();
        let result = Pipeline::from_tokens(&["read".into(), path.to_string_lossy().into_owned()]).unwrap().run().unwrap();
        assert_eq!(result.data.unwrap().vectors().unwrap()[0].features.len(), 2);
        std::fs::write(&path, r#"{"type":"gdal_streamed_alg","command_line":"gdal vector pipeline read plan.gdalg.json"}"#).unwrap();
        assert!(Pipeline::open(path.to_str().unwrap()).unwrap().run().is_err());
    }
    #[test]
    fn saved_definition_inputs_match_native_path_keys() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("plan.gdalg.json");
        let definition = r#"{"type":"gdal_streamed_alg","relative_paths_relative_to_this_file":true,"command_line":"gdal vector pipeline read source.geojson"}"#;
        let plan = Pipeline::from_gdalg(definition, base.to_str()).unwrap();
        let inputs = BTreeMap::from([(dir.path().join("source.geojson").to_string_lossy().into_owned(), 1)]);
        let request = plan.request("source.geojson");
        assert_eq!(request.find_input(&inputs), Some(&1));
    }
    #[test]
    fn native_windows_keys_match_resolved_paths_only_on_windows() {
        assert!(same_path_on(r"C:\d\s.geojson", r"C:\d/s.geojson", true));
        assert!(same_path_on(r"C:\a\s.geojson", r"C:\a\b/../s.geojson", true));
        assert!(same_path_on(r"C:\a\s.geojson", r"C:\a\b/.././s.geojson", true));
        assert!(!same_path_on(r"C:\a\t.geojson", r"C:\a\b/../s.geojson", true));
        assert!(!same_path_on(r"C:\d\s.geojson", r"C:\d/s.geojson", false));
    }
    #[test]
    fn nested_definitions_do_not_write_their_checkpoints() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input.geojson");
        crate::write_vector(vectors().vectors().unwrap(), input.to_str().unwrap(), &Default::default()).unwrap();
        let protected = dir.path().join("protected.geojson");
        std::fs::write(&protected, b"keep existing bytes").unwrap();
        let definition = dir.path().join("nested.gdalg.json");
        let plan = Pipeline::from_tokens(&[
            "read".into(),
            input.to_string_lossy().into_owned(),
            "!".into(),
            "materialize".into(),
            "--output".into(),
            protected.to_string_lossy().into_owned(),
            "--overwrite".into(),
        ])
        .unwrap();
        std::fs::write(&definition, plan.to_gdalg().unwrap()).unwrap();
        let output = dir.path().join("output.geojson");
        Pipeline::from_tokens(&[
            "read".into(),
            definition.to_string_lossy().into_owned(),
            "!".into(),
            "write".into(),
            output.to_string_lossy().into_owned(),
        ])
        .unwrap()
        .run()
        .unwrap();
        assert!(output.exists());
        assert_eq!(std::fs::read(protected).unwrap(), b"keep existing bytes");
    }
    #[test]
    fn resize_rejects_south_up_and_flipped_grids() {
        for gt in [[0., 1., 0., 0., 0., 1.], [2., -1., 0., 2., 0., -1.]] {
            let mut ds = crate::ops::raster_header(
                crate::ops::Grid { width: 2, height: 2, geo_transform: [0., 1., 0., 2., 0., -1.] },
                wbraster::DataType::U8,
                1,
                None,
                &crate::crs::SpatialRef::default(),
            )
            .unwrap();
            ds.affine = Some(gt);
            let error = Pipeline::parse("read a ! resize --size 1,1")
                .unwrap()
                .evaluate_with(|_| Ok(PipelineData::Raster(LazyRaster::from(ds.clone())).into()))
                .err()
                .unwrap();
            assert!(error.to_string().contains("north-up"));
        }
    }
    #[test]
    fn cropped_geolocation_coordinates_match_the_original_pixel_positions() {
        use crate::ops::geolocation::GeolocationTransformer;
        let grid = crate::ops::Grid { width: 4, height: 4, geo_transform: [0., 1., 0., 4., 0., -1.] };
        let mut header =
            crate::ops::raster_header(grid, crate::DataType::F64, 1, None, &crate::crs::SpatialRef::parse("EPSG:4326").unwrap()).unwrap();
        let metadata = crate::dataset::Geolocation {
            srs: "EPSG:4326".into(),
            x_dataset: "longitude".into(),
            x_band: 1,
            y_dataset: "latitude".into(),
            y_band: 1,
            pixel_offset: 0.,
            line_offset: 0.,
            pixel_step: 1.,
            line_step: 1.,
            pixel_center: false,
        };
        header.geolocation = Some(metadata.clone());
        let mut longitude = header.clone();
        longitude.raster.data = wbraster::raster::RasterData::F64((0..16).map(|i| (i % 4) as f64 + 10.).collect());
        let mut latitude = header.clone();
        latitude.raster.data = wbraster::raster::RasterData::F64((0..16).map(|i| 40. - (i / 4) as f64).collect());
        let original = GeolocationTransformer::new(&metadata, &longitude, &latitude, 4, 4).unwrap();
        let view = raster::view(LazyRaster::from(header), vec![], Some(Window::new(1, 2, 2, 2))).unwrap();
        let cropped = GeolocationTransformer::new(view.header().geolocation.as_ref().unwrap(), &longitude, &latitude, 2, 2).unwrap();
        assert_eq!(cropped.to_geo(0.5, 0.5), original.to_geo(1.5, 2.5));
        for bbox in ["10,0,0,10", "0,10,10,0", "0,0,0,1"] {
            assert!(Pipeline::parse(&format!("read alias ! rasterize --size 2,2 --extent {bbox}")).is_err());
        }
    }
    #[test]
    fn resize_interpolates_raw_pixels_under_a_dataset_mask() {
        let dir = tempfile::tempdir().unwrap();
        let grid = crate::ops::Grid { width: 4, height: 4, geo_transform: [0., 1., 0., 4., 0., -1.] };
        let values = (0..16).map(|v| (v * 10) as u8).collect();
        let ds = crate::ops::build_raster_data(grid, wbraster::raster::RasterData::U8(values), 1, None, &Default::default()).unwrap();
        let (raw, masked) = (dir.path().join("raw.tif"), dir.path().join("masked.tif"));
        crate::write_raster_dataset(&ds, raw.to_str().unwrap(), &Default::default()).unwrap();
        let mut mask = vec![255; 16];
        mask[5] = 0;
        let options = crate::RasterWriteOptions { mask: Some(mask), ..Default::default() };
        crate::write_raster_dataset(&ds, masked.to_str().unwrap(), &options).unwrap();
        let options =
            crate::ops::warp::WarpOptions { size: Some((2, 2)), resampling: crate::ops::Resampling::Bilinear, ..Default::default() };
        let mut raw = LazyRaster::open(raw.to_str().unwrap()).unwrap();
        let expected = crate::ops::warp::warp_lazy(&mut raw, &options).unwrap();
        let mut masked = LazyRaster::open(masked.to_str().unwrap()).unwrap();
        let mut resized = raster::resize(&mut masked, &options).unwrap();
        assert_eq!(resized.read().unwrap().raster.data.to_f64_vec(), expected.raster.data.to_f64_vec());
        assert!(resized.has_mask());
    }
    #[test]
    fn crop_and_band_selection_only_read_requested_source_window() {
        use std::sync::{Arc, Mutex};
        type ReadLog = Arc<Mutex<Vec<(usize, Window, Vec<usize>)>>>;
        struct Counted {
            reads: ReadLog,
        }
        impl crate::cog::WindowSource for Counted {
            fn overviews(&self) -> Vec<(usize, usize)> {
                vec![(5, 5)]
            }
            fn read(&mut self, level: usize, window: Window, bands: &[usize]) -> Result<wbraster::raster::RasterData> {
                self.reads.lock().unwrap().push((level, window, bands.to_vec()));
                Ok(wbraster::raster::RasterData::U64(vec![9007199254740993; window.width * window.height * bands.len()]))
            }
        }
        let reads = Arc::new(Mutex::new(vec![]));
        let header = crate::ops::raster_header(
            crate::ops::Grid { width: 10, height: 10, geo_transform: [0., 1., 0., 10., 0., -1.] },
            wbraster::DataType::U64,
            2,
            None,
            &crate::crs::SpatialRef::parse("EPSG:4326").unwrap(),
        )
        .unwrap();
        let mut source = LazyRaster::from_source(header, Box::new(Counted { reads: reads.clone() }));
        let result = Pipeline::parse("read alias ! clip --bbox 2,2,7,8 ! select 2")
            .unwrap()
            .evaluate_with(|_| Ok(PipelineData::Raster(source.shared()).into()))
            .unwrap();
        assert!(reads.lock().unwrap().is_empty(), "parsing and view creation must not read pixels");
        let PipelineData::Raster(mut raster) = result.data.unwrap() else { panic!("expected raster") };
        assert_eq!(raster.overviews(), vec![(3, 3)]);
        let sample = raster.read_window(Window::new(1, 1, 2, 2), &Default::default()).unwrap();
        assert!(matches!(sample.raster.data, wbraster::raster::RasterData::U64(values) if values == vec![9007199254740993; 4]));
        assert_eq!(*reads.lock().unwrap(), vec![(0, Window::new(3, 3, 2, 2), vec![2])]);
        let overview = raster.read_window(Window::new(0, 0, 2, 2), &crate::WindowOptions { bands: vec![1], overview: Some(0) }).unwrap();
        assert!(matches!(overview.raster.data, wbraster::raster::RasterData::U64(values) if values == vec![9007199254740993; 4]));
        assert_eq!(reads.lock().unwrap()[1], (1, Window::new(1, 1, 2, 2), vec![2]));
    }
    #[test]
    fn raster_outputs_preserve_masks_and_absent_geotransforms() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("unreferenced.tif");
        let mut ds = crate::ops::raster_header(
            crate::ops::Grid { width: 2, height: 2, geo_transform: [0., 1., 0., 2., 0., -1.] },
            wbraster::DataType::U8,
            1,
            None,
            &crate::crs::SpatialRef::default(),
        )
        .unwrap();
        ds.raster.data = wbraster::raster::RasterData::U8(vec![1, 2, 3, 4]);
        ds.band_metadata = vec![crate::BandMetadata {
            description: Some("elevation".into()),
            items: vec![("quality".into(), "surveyed".into()), ("STATISTICS_MINIMUM".into(), "1".into())],
            ..Default::default()
        }];
        let mask = vec![0, 255, 255, 0];
        let options = crate::RasterWriteOptions {
            mask: Some(mask.clone()),
            creation_options: vec![("GEOREFERENCED".into(), "NO".into())],
            ..Default::default()
        };
        crate::write_raster_dataset(&ds, input.to_str().unwrap(), &options).unwrap();
        let mut unreferenced = LazyRaster::open(input.to_str().unwrap()).unwrap();
        let mut png_options = crate::RasterWriteOptions::default();
        unreferenced.prepare_output_mask("unreferenced.png", &mut png_options, Window::full(2, 2), None).unwrap();
        assert!(!png_options.creation_options.iter().any(|(key, _)| key == "GEOREFERENCED"));
        // Shared writers such as `palette` must not gain the pipeline's absent-transform option.
        let mut tif_options = crate::RasterWriteOptions::default();
        unreferenced.prepare_output_mask("unreferenced.tif", &mut tif_options, Window::full(2, 2), None).unwrap();
        assert!(!tif_options.creation_options.iter().any(|(key, _)| key == "GEOREFERENCED"));

        for (index, middle) in [
            vec![],
            vec!["select", "1"],
            vec!["materialize"],
            vec!["set-type", "--output-data-type", "Float32"],
            vec!["resize", "--size", "2,2"],
        ]
        .iter()
        .enumerate()
        {
            let output = dir.path().join(format!("result-{index}.tif"));
            let mut tokens = vec!["read".to_string(), input.to_string_lossy().into_owned()];
            if !middle.is_empty() {
                tokens.push("!".into());
                tokens.extend(middle.iter().map(|s| s.to_string()));
            }
            tokens.extend(["!".into(), "write".into(), output.to_string_lossy().into_owned()]);
            let mut result = Pipeline::from_tokens(&tokens).unwrap().run().unwrap();
            let mut raster = LazyRaster::open(output.to_str().unwrap()).unwrap();
            let metadata = raster.header().band_metadata;
            assert!(metadata[0].items.contains(&("quality".into(), "surveyed".into())), "case {index}");
            assert_eq!(metadata[0].items.iter().any(|(key, _)| key == "STATISTICS_MINIMUM"), index < 3, "case {index}");
            assert!(!raster.geotransform_present(), "case {index}");
            assert_eq!(raster.read_mask(Window::full(2, 2), None).unwrap(), Some(mask.clone()), "case {index}");
            let bytes = result.output_bytes().unwrap();
            let mut encoded = crate::memory::open_raster_lazy_bytes(bytes[output.to_str().unwrap()].clone(), "result.tif").unwrap();
            assert!(!encoded.geotransform_present(), "encoded case {index}");
            assert_eq!(encoded.read_mask(Window::full(2, 2), None).unwrap(), Some(mask.clone()));
        }
    }
    #[test]
    fn direct_raster_exports_keep_absent_transforms_and_gcps() {
        let dir = tempfile::tempdir().unwrap();
        for with_gcps in [false, true] {
            let mut ds = crate::ops::raster_header(
                crate::ops::Grid { width: 2, height: 2, geo_transform: [0., 1., 0., 2., 0., -1.] },
                wbraster::DataType::U8,
                1,
                None,
                &crate::crs::SpatialRef::default(),
            )
            .unwrap();
            ds.raster.data = wbraster::raster::RasterData::U8(vec![1, 2, 3, 4]);
            if with_gcps {
                ds.gcps.push(crate::Gcp { id: "control".into(), info: String::new(), pixel: 0.5, line: 0.5, x: 10., y: 20., z: 30. });
            }
            let input = dir.path().join(format!("source-{with_gcps}.tif"));
            let options = crate::RasterWriteOptions { creation_options: vec![("GEOREFERENCED".into(), "NO".into())], ..Default::default() };
            crate::write_raster_dataset(&ds, input.to_str().unwrap(), &options).unwrap();
            let mut reader = LazyRaster::open(input.to_str().unwrap()).unwrap();
            let original = reader.header();
            assert_eq!(original.gcps.len(), usize::from(with_gcps));
            assert!(!reader.geotransform_present());
            let output = dir.path().join(format!("copy-{with_gcps}.tif"));
            let mut options = crate::RasterWriteOptions::default();
            keep_unreferenced(&reader, &mut options, output.to_str().unwrap());
            reader.prepare_output_mask(output.to_str().unwrap(), &mut options, Window::full(2, 2), None).unwrap();
            reader.write(output.to_str().unwrap(), &options).unwrap();
            let mut copy = LazyRaster::open(output.to_str().unwrap()).unwrap();
            assert!(!copy.geotransform_present());
            assert_eq!(copy.header().gcps, original.gcps);
            assert!(!copy.header().raster.metadata.iter().any(|(key, _)| key == "GEOREFERENCED"));
            assert_eq!(copy.read().unwrap().raster.data.to_f64_vec(), [1., 2., 3., 4.]);
        }
    }
    #[test]
    fn raster_band_copy_is_exact_for_u64_and_safe_when_overwriting_input() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("values.tif");
        let mut ds = crate::ops::raster_header(
            crate::ops::Grid { width: 2, height: 2, geo_transform: [0., 1., 0., 2., 0., -1.] },
            wbraster::DataType::U64,
            1,
            None,
            &crate::crs::SpatialRef::parse("EPSG:4326").unwrap(),
        )
        .unwrap();
        let values = vec![9007199254740993, 9007199254740995, u64::MAX - 1, 1];
        ds.raster.data = wbraster::raster::RasterData::U64(values.clone());
        crate::write_raster_dataset(&ds, path.to_str().unwrap(), &Default::default()).unwrap();
        let tokens = vec![
            "raster".into(),
            "read".into(),
            path.to_string_lossy().into_owned(),
            "!".into(),
            "select".into(),
            "1".into(),
            "!".into(),
            "write".into(),
            path.to_string_lossy().into_owned(),
            "--overwrite".into(),
        ];
        let result = Pipeline::from_tokens(&tokens).unwrap().run().unwrap();
        let Dataset::Raster(mut output) = result.data.unwrap().into_dataset().unwrap() else { panic!("raster") };
        assert!(matches!(&output.raster.data,wbraster::raster::RasterData::U64(v) if v==&values));
        output = crate::open_raster(path.to_str().unwrap()).unwrap();
        assert!(matches!(&output.raster.data,wbraster::raster::RasterData::U64(v) if v==&values));
    }
}
