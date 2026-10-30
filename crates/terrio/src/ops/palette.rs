//! RGB quantization and palette mapping following GDAL 3.13.3.
//! Median-cut ordering and integer diffusion follow gdalmediancut.cpp/gdaldither.cpp (NOTICE).
use crate::{BandMetadata, Error, RasterDataset, Result};
use std::collections::HashMap;
use wbraster::raster::RasterData;

/// Options for converting RGB bands to a Byte palette index.
#[derive(Debug, Clone)]
pub struct PaletteOptions {
    /// Number of generated colors, from 2 through 256.
    pub color_count: usize,
    /// Histogram/color lookup precision: 5 (default) or 8 bits per component.
    pub bit_depth: u8,
    /// Apply GDAL's integer error diffusion (default true).
    pub dither: bool,
    /// A supplied RGBA palette instead of median-cut generation.
    pub color_table: Option<Vec<[u8; 4]>>,
    /// Reserve this index for invalid source pixels. Auto-select 0 when generating
    /// a palette from sources with a shared RGB nodata value or an explicit mask.
    pub output_nodata: Option<u8>,
    /// Optional source validity, one byte per pixel; zero is invalid. Shared
    /// RGB nodata takes precedence over this mask (including caller masks),
    /// following GDAL. A supplied table needs output_nodata to mark invalid pixels.
    pub mask: Option<Vec<u8>>,
}
impl Default for PaletteOptions {
    fn default() -> Self {
        Self { color_count: 256, bit_depth: 5, dither: true, color_table: None, output_nodata: None, mask: None }
    }
}
fn invalid(message: impl Into<String>) -> Error {
    Error::InvalidArgument(message.into())
}
#[derive(Clone)]
struct Bin {
    color: [u8; 3],
    count: u64,
}
struct ColorBox {
    bins: Vec<Bin>,
    total: u64,
    lo: [i32; 3],
    hi: [i32; 3],
}
impl ColorBox {
    fn shrink(&mut self) {
        if self.bins.is_empty() {
            return;
        }
        self.lo = [255; 3];
        self.hi = [0; 3];
        for b in &self.bins {
            for i in 0..3 {
                self.lo[i] = self.lo[i].min(b.color[i] as i32);
                self.hi[i] = self.hi[i].max(b.color[i] as i32);
            }
        }
    }
    fn bounds(&self) -> ([i32; 3], [i32; 3]) {
        (self.lo, self.hi)
    }
    fn splittable(&self) -> bool {
        (0..3).any(|i| self.hi[i] > self.lo[i])
    }
}
// Sparse occupied histogram bins give the same marginal sums/shrunk bounds as
// GDAL's dense cube, including its linked-list traversal and tie ordering.
fn median_cut(rgb: &[[u8; 3]], valid: &[bool], count: usize, bits: u8) -> Vec<[u8; 4]> {
    let shift = 8 - bits;
    let mut bins: Vec<Bin> = Vec::new();
    let mut lookup = HashMap::new();
    for (color, valid) in rgb.iter().zip(valid) {
        if !valid {
            continue;
        }
        let c = color.map(|v| v >> shift);
        let next = bins.len();
        let index = *lookup.entry(c).or_insert(next);
        if index == next {
            bins.push(Bin { color: c, count: 0 });
        }
        bins[index].count += 1;
    }
    if bits == 8 && bins.len() <= count {
        return bins.iter().map(|b| [b.color[0], b.color[1], b.color[2], 255]).collect();
    }
    if bins.is_empty() {
        return Vec::new();
    }
    let mut first = ColorBox { bins, total: rgb.len() as u64, lo: [0; 3], hi: [0; 3] };
    first.shrink();
    let mut boxes = vec![first];
    while boxes.len() < count {
        let mut largest = None;
        for (i, b) in boxes.iter().enumerate() {
            if b.splittable() && largest.is_none_or(|j: usize| b.total > boxes[j].total) {
                largest = Some(i);
            }
        }
        let Some(index) = largest else {
            break;
        };
        let (lo, hi) = boxes[index].bounds();
        let delta = std::array::from_fn::<_, 3, _>(|i| hi[i] - lo[i]);
        let axis = if delta[0] >= delta[1] && delta[0] >= delta[2] {
            0
        } else if delta[1] >= delta[2] {
            1
        } else {
            2
        };
        let mut marginal = [0u64; 256];
        for bin in &boxes[index].bins {
            marginal[bin.color[axis] as usize] += bin.count;
        }
        let mut cut = lo[axis] as usize;
        let mut sum = 0;
        while cut <= hi[axis] as usize {
            sum += marginal[cut];
            if sum >= boxes[index].total / 2 {
                break;
            }
            cut += 1;
        }
        if cut == lo[axis] as usize {
            cut += 1;
        }
        // GDAL includes invalid pixels in the initial total. Preserve empty
        // box bounds too: heavily masked images can therefore get unused
        // palette entries. Changing the total changes their color ordering.
        let (lower, upper): (Vec<_>, Vec<_>) =
            std::mem::take(&mut boxes[index].bins).into_iter().partition(|b| (b.color[axis] as usize) < cut);
        let total = upper.iter().map(|b| b.count).sum();
        let mut upper = ColorBox { bins: upper, total, lo, hi };
        upper.lo[axis] = cut as i32;
        upper.shrink();
        boxes[index] = upper;
        let total = lower.iter().map(|b| b.count).sum();
        let mut lower = ColorBox { bins: lower, total, lo, hi };
        lower.hi[axis] = cut as i32 - 1;
        lower.shrink();
        boxes.insert(0, lower);
    }
    boxes
        .iter()
        .map(|b| {
            let (lo, hi) = b.bounds();
            let c = std::array::from_fn::<_, 3, _>(|i| (((lo[i] + hi[i]) << shift) / 2) as u8);
            [c[0], c[1], c[2], 255]
        })
        .collect()
}
fn nearest(color: [u8; 3], table: &[[u8; 4]], nodata: Option<u8>) -> u8 {
    let (mut best, mut distance) = (0, 768u32);
    for (i, c) in table.iter().enumerate() {
        if nodata == Some(i as u8) {
            continue;
        }
        let d = (0..3).map(|k| color[k].abs_diff(c[k]) as u32).sum();
        if d < distance {
            best = i as u8;
            distance = d;
        }
    }
    best
}
fn map_pixels(
    rgb: &[[u8; 3]],
    valid: &[bool],
    width: usize,
    table: &[[u8; 4]],
    options: &PaletteOptions,
    nodata: Option<u8>,
    source_nodata: Option<u8>,
) -> Vec<u8> {
    let mut errors = vec![[0i32; 3]; width + 2];
    // Small full-precision images need at most one entry per pixel. Larger
    // images use a dense cache to avoid a cap-and-miss cliff; 5-bit queries
    // always fit in a 64 KiB dense table.
    let mut small_cache = (options.bit_depth == 8 && rgb.len() <= 65_536).then(HashMap::new);
    let mut cache = if small_cache.is_none() { vec![u16::MAX; (1usize << options.bit_depth).pow(3)] } else { Vec::new() };
    let mut output = Vec::with_capacity(rgb.len());
    let levels = 1u32 << options.bit_depth;
    for (y, row) in rgb.chunks_exact(width).enumerate() {
        let mut values: Vec<_> = row
            .iter()
            .enumerate()
            .map(|(x, c)| {
                std::array::from_fn::<_, 3, _>(|k| {
                    let adjustment = if options.dither && (nodata.is_none() || valid[y * width + x]) { errors[x + 1][k] } else { 0 };
                    (c[k] as i32 + adjustment).clamp(0, 255) as u8
                })
            })
            .collect();
        errors.fill([0; 3]);
        let mut carry = [0; 3];
        for (x, c) in values.iter_mut().enumerate() {
            // GDAL also tests the vertically diffused values for source nodata.
            // This can map an originally valid near-black pixel to nodata.
            if let Some(nodata) = nodata
                && (!valid[y * width + x] || source_nodata.is_some_and(|nd| c.iter().all(|v| *v == nd)))
            {
                output.push(nodata);
                carry = [0; 3];
                continue;
            }
            for k in 0..3 {
                c[k] = (c[k] as i32 + carry[k]).clamp(0, 255) as u8;
            }
            // GDAL's fixed color cube maps floor(value*levels/256) to
            // floor(cube_coordinate*255/(levels-1)), even without dithering.
            let query = c.map(|v| (((v as u32 * levels / 256) * 255) / (levels - 1)) as u8);
            let cube = c.map(|v| v as usize * levels as usize / 256);
            let key = (cube[0] * levels as usize + cube[1]) * levels as usize + cube[2];
            let index = if let Some(cache) = &mut small_cache {
                *cache.entry(query).or_insert_with(|| nearest(query, table, nodata))
            } else if cache[key] == u16::MAX {
                let index = nearest(query, table, nodata);
                cache[key] = index as u16;
                index
            } else {
                cache[key] as u8
            };
            output.push(index);
            if options.dither {
                for k in 0..3 {
                    let error = c[k] as i32 - table[index as usize][k] as i32;
                    let sixth = error / 6;
                    errors[x][k] += sixth;
                    errors[x + 2][k] = sixth;
                    errors[x + 1][k] += error - 5 * sixth;
                    carry[k] = 2 * sixth;
                }
            }
        }
    }
    output
}
/// Converts a lazy source, automatically reading its per-dataset validity mask.
/// The quantizer currently materializes the source and output; ordinary lazy
/// info/window reads are unaffected. Alpha validity is handled by the core.
pub fn rgb_to_palette_lazy(source: &mut crate::LazyRaster, options: &PaletteOptions) -> Result<RasterDataset> {
    let mut options = options.clone();
    if options.mask.is_none() && source.has_mask() && (options.color_table.is_none() || options.output_nodata.is_some()) {
        options.mask = source.read_mask(crate::Window::full(source.width(), source.height()), None)?;
    }
    rgb_to_palette(&source.read()?, &options)
}
/// Returns a raster palette with nodata transparency applied, as GDAL exposes it.
pub fn color_table(source: &RasterDataset) -> Result<Vec<[u8; 4]>> {
    let mut table = source.color_table.clone();
    if table.is_empty() {
        return Err(invalid("palette raster has no color table"));
    }
    if let Some(value) = source.band_nodata(1)
        && value.is_finite()
        && value >= 0.
        && value.fract() == 0.
        && let Some(color) = table.get_mut(value as usize)
    {
        color[3] = 0;
    }
    Ok(table)
}
/// Loads a color table from an uploaded raster or indexed color-file bytes.
/// Raster readers inspect headers where supported, without decoding pixels.
pub fn color_table_from_bytes(bytes: Vec<u8>, filename: &str) -> Result<Vec<[u8; 4]>> {
    let ext = crate::formats::extension(filename).unwrap_or_default();
    if ["txt", "qml", "qlr", "cpt", "pal", "csv", "xml"].contains(&ext.as_str()) {
        return parse_color_table(std::str::from_utf8(&bytes).map_err(|_| invalid("color-file text must be UTF-8"))?, filename);
    }
    let source = crate::memory::open_raster_lazy_bytes(bytes, filename)?;
    color_table(&source.header())
}
/// Loads a native/VSI raster palette or indexed text/QGIS color file.
#[cfg(not(target_family = "wasm"))]
pub fn read_color_table(path: &str) -> Result<Vec<[u8; 4]>> {
    let ext = crate::formats::extension(path).unwrap_or_default();
    if ["txt", "qml", "qlr", "cpt", "pal", "csv", "xml"].contains(&ext.as_str()) {
        if crate::vsi::stat(path)?.and_then(|entry| entry.size).is_some_and(|size| size > 10 * 1024 * 1024) {
            return Err(invalid("color table text exceeds 10 MiB"));
        }
        let bytes = crate::vsi::read_limited(path, 10 * 1024 * 1024).map_err(|error| match error {
            crate::Error::DecodedLimitExceeded { resource: "VSI file", .. } => invalid("color table text exceeds 10 MiB"),
            other => other,
        })?;
        return color_table_from_bytes(bytes, path);
    }
    let source = crate::open_raster_lazy(path)?;
    color_table(&source.header())
}
/// Native files supplying a color table, including raster sidecars.
#[cfg(not(target_family = "wasm"))]
pub fn color_table_files(path: &str) -> Result<Vec<String>> {
    let mut files = vec![path.to_string()];
    let ext = crate::formats::extension(path).unwrap_or_default();
    if !["txt", "qml", "qlr", "cpt", "pal", "csv", "xml"].contains(&ext.as_str()) {
        files.extend(crate::open_raster_lazy(path)?.file_list()?);
    }
    Ok(files)
}
/// Quantizes RGB Byte bands, preserving the source grid and dataset metadata.
/// Color interpretations select R/G/B when all are present, otherwise bands 1/2/3.
/// Invalid pixels are excluded from histogram generation. A declared alpha band
/// supplies validity when there is no shared RGB nodata value or explicit mask.
pub fn rgb_to_palette(source: &RasterDataset, options: &PaletteOptions) -> Result<RasterDataset> {
    if ![5, 8].contains(&options.bit_depth) || (options.color_table.is_none() && !(2..=256).contains(&options.color_count)) {
        return Err(invalid("palette needs 2..256 colors and bit depth 5 or 8"));
    }
    let n = source.raster.rows.checked_mul(source.raster.cols).ok_or_else(|| invalid("raster size overflow"))?;
    let RasterData::U8(data) = &source.raster.data else {
        return Err(invalid("RGB-to-palette requires Byte bands"));
    };
    if n == 0 || source.raster.bands < 3 || source.complex || n.checked_mul(source.raster.bands) != Some(data.len()) {
        return Err(invalid("RGB-to-palette needs at least three nonempty Byte bands"));
    }
    if options.mask.as_ref().is_some_and(|m| m.len() != n) {
        return Err(invalid("palette mask must contain one value per pixel"));
    }
    let mut bands = [0, 1, 2];
    let mut found = [None; 3];
    for (i, b) in source.band_metadata.iter().take(source.raster.bands).enumerate() {
        for (k, name) in ["red", "green", "blue"].iter().enumerate() {
            if b.color_interp.as_deref().is_some_and(|c| c.eq_ignore_ascii_case(name)) && found[k].replace(i).is_some() {
                return Err(invalid("duplicate RGB color interpretation"));
            }
        }
    }
    if found.iter().all(Option::is_some) {
        bands = found.map(Option::unwrap);
    }
    let nd = source.band_nodata(bands[0] + 1);
    let nd = nd.filter(|v| {
        v.is_finite() && *v >= 0. && *v <= 255. && v.fract() == 0. && bands.iter().all(|b| source.band_nodata(*b + 1) == Some(*v))
    });
    let alpha = source
        .band_metadata
        .iter()
        .take(source.raster.bands)
        .position(|b| b.color_interp.as_deref().is_some_and(|c| c.eq_ignore_ascii_case("alpha")));
    let mask = options.mask.as_deref().or_else(|| alpha.map(|b| &data[b * n..(b + 1) * n]));
    let rgb: Vec<_> = (0..n).map(|i| bands.map(|b| data[b * n + i])).collect();
    // GDAL gives shared RGB nodata precedence over alpha/dataset masks.
    // Keeping both exclusions would change its palette and output indices.
    let valid: Vec<_> = rgb
        .iter()
        .enumerate()
        .map(|(i, c)| if let Some(nd) = nd { c.iter().any(|v| *v != nd as u8) } else { mask.is_none_or(|m| m[i] != 0) })
        .collect();
    let nodata = options.output_nodata.or_else(|| (options.color_table.is_none() && (nd.is_some() || mask.is_some())).then_some(0));
    let count = if nodata.is_some() && options.color_count == 256 { 255 } else { options.color_count };
    let mut table = if let Some(table) = &options.color_table {
        if table.is_empty() || table.len() > 256 {
            return Err(invalid("a supplied palette needs 1..256 entries"));
        }
        table.clone()
    } else {
        median_cut(&rgb, &valid, count, options.bit_depth)
    };
    if let Some(index) = nodata {
        let index = index as usize;
        if table.len() > index {
            table.insert(index, [0; 4]);
            table.truncate(256);
        } else {
            table.resize(index + 1, [0; 4]);
        }
    }
    // An all-invalid input retains the reserved nodata entry, matching GDAL.
    let pixels = map_pixels(&rgb, &valid, source.raster.cols, &table, options, nodata, nd.map(|v| v as u8));
    let mut out = super::derived(source, RasterData::U8(pixels), 1)?;
    out.color_table = table;
    out.has_nodata = nodata.is_some();
    out.band_nodata = vec![nodata.map(f64::from)];
    if let Some(nodata) = nodata {
        out.raster.nodata = nodata as f64;
    }
    out.band_metadata = vec![BandMetadata { color_interp: Some("Palette".into()), ..Default::default() }];
    Ok(out)
}
/// Expands one palette-index band to RGB or RGBA Byte bands. Missing entries
/// are black in RGB and transparent black in RGBA. Source nodata is not copied.
pub fn palette_to_rgb(source: &RasterDataset, alpha: bool) -> Result<RasterDataset> {
    if source.raster.bands != 1 || source.color_table.is_empty() || source.complex {
        return Err(invalid("palette expansion requires one indexed band and a color table"));
    }
    let bands = if alpha { 4 } else { 3 };
    let n = source.raster.rows.checked_mul(source.raster.cols).ok_or_else(|| invalid("raster size overflow"))?;
    if source.raster.data.len() != n {
        return Err(invalid("palette expansion requires one value per pixel"));
    }
    let length = n.checked_mul(bands).ok_or_else(|| invalid("palette output size overflow"))?;
    let mut data = vec![0; length];
    let nodata = source.band_nodata(1);
    for i in 0..n {
        let v = source.raster.data.get_f64(i);
        let mut color = if v.is_finite() && v >= 0. && v.fract() == 0. {
            source.color_table.get(v as usize).copied().unwrap_or([0; 4])
        } else {
            [0; 4]
        };
        if nodata == Some(v) {
            color[3] = 0;
        }
        for b in 0..bands {
            data[b * n + i] = color[b];
        }
    }
    let mut out = super::derived(source, RasterData::U8(data), bands)?;
    out.band_metadata = ["Red", "Green", "Blue", "Alpha"]
        .iter()
        .take(bands)
        .map(|c| BandMetadata { color_interp: Some((*c).into()), ..Default::default() })
        .collect();
    Ok(out)
}
fn convert_with_mask(
    source: &mut crate::LazyRaster,
    convert: impl FnOnce(&RasterDataset) -> Result<RasterDataset>,
) -> Result<crate::LazyRaster> {
    let mask = source.read_mask(crate::Window::full(source.width(), source.height()), None)?;
    let output = convert(&source.read()?)?;
    match mask {
        Some(mask) => crate::LazyRaster::from_masked_dataset(output, mask),
        None => Ok(output.into()),
    }
}
/// Expands a palette while retaining the source's per-dataset mask in the returned raster.
/// RGBA expansion also makes invalid pixels transparent, preserving validity in PNG bytes.
/// The conversion materializes the full input and output.
pub fn palette_to_rgb_lazy(source: &mut crate::LazyRaster, alpha: bool) -> Result<crate::LazyRaster> {
    let mask = source.read_mask(crate::Window::full(source.width(), source.height()), None)?;
    let mut output = palette_to_rgb(&source.read()?, alpha)?;
    if alpha && let Some(mask) = &mask {
        let n = output.raster.rows * output.raster.cols;
        if mask.len() != n {
            return Err(invalid("palette mask must contain one value per pixel"));
        }
        if let RasterData::U8(data) = &mut output.raster.data {
            for (i, &valid) in mask.iter().enumerate() {
                if valid == 0 {
                    data[3 * n + i] = 0;
                }
            }
        }
    }
    match mask {
        Some(mask) => crate::LazyRaster::from_masked_dataset(output, mask),
        None => Ok(output.into()),
    }
}
/// Attaches a table while retaining the source's per-dataset mask in the returned raster.
/// The conversion materializes the full input and output.
pub fn attach_palette_lazy(source: &mut crate::LazyRaster, table: &[[u8; 4]]) -> Result<crate::LazyRaster> {
    convert_with_mask(source, |dataset| attach_palette(dataset, table))
}
/// Checks supported output locations, replacement and native aliases of known inputs.
#[cfg(not(target_family = "wasm"))]
pub fn check_output(path: &str, sources: &[&str], overwrite: bool) -> Result<()> {
    crate::dataset::ensure_writable_location(path)?;
    let output = std::path::Path::new(path);
    if (std::fs::symlink_metadata(output).is_ok() || crate::memfs::exists(output)) && !overwrite {
        return Err(invalid("output exists; enable overwrite to replace it"));
    }
    for source in sources {
        let input = std::path::Path::new(source);
        let same =
            input == output || std::fs::canonicalize(input).ok().zip(std::fs::canonicalize(output).ok()).is_some_and(|(a, b)| a == b);
        #[cfg(unix)]
        let same = same || {
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(input).ok().zip(std::fs::metadata(output).ok()).is_some_and(|(a, b)| a.dev() == b.dev() && a.ino() == b.ino())
        };
        #[cfg(windows)]
        // New destinations have no file identity; their NotFound error permits creation.
        let same = same || same_file::is_same_file(input, output).unwrap_or(false);
        if same {
            return Err(invalid("palette output cannot replace an input or its alias"));
        }
    }
    Ok(())
}
/// Parses an indexed GDAL/GRASS text, RGB GMT CPT, or QGIS QML/QLR color table.
/// Entries must have integer indices in 0..=65535. Unspecified entries are
/// transparent black. Unlike GDAL 3.13.3's QML alpha parsing bug, explicit XML
/// alpha values are honored.
pub fn parse_color_table(text: &str, filename: &str) -> Result<Vec<[u8; 4]>> {
    if text.len() > 10 * 1024 * 1024 {
        return Err(invalid("color table text exceeds 10 MiB"));
    }
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut entries = Vec::new();
    let ext = filename.rsplit('.').next().unwrap_or("");
    if ext.eq_ignore_ascii_case("qml") || ext.eq_ignore_ascii_case("qlr") || text.trim_start().starts_with('<') {
        let root = crate::drivers::vector::xml::parse_limited(text, 65_536)?;
        let renderer = root.find("rasterrenderer").ok_or_else(|| invalid("no raster renderer in QGIS color file"))?;
        let tag = match renderer.attr("type") {
            Some("paletted") => "paletteEntry",
            Some("singlebandpseudocolor") => "item",
            _ => return Err(invalid("QGIS color table needs a paletted or singlebandpseudocolor renderer")),
        };
        let mut nodes = Vec::new();
        renderer.find_all(tag, &mut nodes);
        for node in nodes {
            let index = node
                .attr("value")
                .ok_or_else(|| invalid("missing palette value"))?
                .parse::<usize>()
                .map_err(|_| invalid("palette values must be integer indices"))?;
            let color = node.attr("color").unwrap_or("");
            if color.len() != 7 || !color.starts_with('#') {
                return Err(invalid("QGIS palette colors must use #RRGGBB"));
            }
            let rgb = u32::from_str_radix(&color[1..], 16).map_err(|_| invalid("invalid QGIS palette color"))?;
            let alpha =
                node.attr("alpha").map(|s| s.parse::<u8>()).transpose().map_err(|_| invalid("invalid QGIS palette alpha"))?.unwrap_or(255);
            entries.push((index, [(rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8, alpha]));
        }
    } else {
        if text.lines().map(str::trim).filter(|line| !line.starts_with(['#', '/'])).any(|line| line.contains('%')) {
            return Err(invalid("palette indices cannot be percentages"));
        }
        let header = super::raster_header(
            super::Grid { width: 1, height: 1, geo_transform: [0., 1., 0., 1., 0., -1.] },
            crate::DataType::U8,
            1,
            None,
            &Default::default(),
        )?;
        let map = super::dem::ColorMap::parse(text, &header, 1, None)?;
        for entry in map.entries {
            if !entry.value.is_finite() || entry.value < 0. || entry.value.fract() != 0. || entry.value > 65535. {
                return Err(invalid("palette values must be integer indices in 0..65535"));
            }
            let byte = |value: i32| u8::try_from(value).map_err(|_| invalid("color channel must be 0..255"));
            entries.push((entry.value as usize, [byte(entry.r)?, byte(entry.g)?, byte(entry.b)?, byte(entry.a)?]));
        }
    }
    let mut output = Vec::new();
    for (index, color) in entries {
        if index > 65535 {
            return Err(invalid("palette index exceeds 65535"));
        }
        if output.len() <= index {
            output.resize(index + 1, [0; 4]);
        }
        output[index] = color;
    }
    if output.is_empty() {
        return Err(invalid("color table has no entries"));
    }
    Ok(output)
}
/// Attaches a supplied RGBA table without altering source pixel values.
pub fn attach_palette(source: &RasterDataset, table: &[[u8; 4]]) -> Result<RasterDataset> {
    if source.raster.bands != 1 || source.complex || table.is_empty() {
        return Err(invalid("palette attachment requires one indexed band and a nonempty color table"));
    }
    if !matches!(source.raster.data, RasterData::U8(_) | RasterData::U16(_)) {
        return Err(invalid("palette attachment requires Byte or UInt16 indices"));
    }
    let limit = if matches!(source.raster.data, RasterData::U16(_)) { 65536 } else { 256 };
    if table.len() > limit {
        return Err(invalid("color table has more entries than the index type can address"));
    }
    let mut out = source.clone();
    out.color_table = table.to_vec();
    out.band_metadata.resize_with(1, Default::default);
    out.band_metadata[0].color_interp = Some("Palette".into());
    Ok(out)
}

#[cfg(test)]
mod tests {
    #[cfg(not(target_family = "wasm"))]
    #[test]
    fn native_hard_link_outputs_cannot_replace_sources() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("source.tif");
        let alias = directory.path().join("alias.tif");
        let independent = directory.path().join("independent.tif");
        std::fs::write(&input, b"original").unwrap();
        std::fs::hard_link(&input, &alias).unwrap();
        std::fs::write(&independent, b"original").unwrap();
        let sources = [input.to_str().unwrap()];
        let error = check_output(alias.to_str().unwrap(), &sources, true).unwrap_err();
        assert!(error.to_string().contains("input or its alias"));
        assert!(check_output(independent.to_str().unwrap(), &sources, true).is_ok());
        assert!(check_output(directory.path().join("new.tif").to_str().unwrap(), &sources, false).is_ok());
        assert_eq!(std::fs::read(input).unwrap(), b"original");
        assert_eq!(std::fs::read(alias).unwrap(), b"original");
    }

    #[cfg(not(target_family = "wasm"))]
    #[test]
    fn memory_output_requires_overwrite_and_remote_outputs_are_refused() {
        let path = "/vsimem/palette-output-guard.tif";
        let p = std::path::Path::new(path);
        crate::memfs::write(p, b"original").unwrap();
        assert!(super::check_output(path, &[], false).unwrap_err().to_string().contains("output exists"));
        assert!(super::check_output(path, &[], true).is_ok());
        assert!(super::check_output(path, &[path], true).is_err());
        assert_eq!(crate::memfs::read(p).unwrap(), b"original");
        assert!(crate::memfs::remove_file(p));
        for path in ["s3://bucket/output.tif", "https://example.com/output.tif"] {
            assert!(super::check_output(path, &[], true).unwrap_err().to_string().contains("only local outputs"));
        }
    }
    use super::*;
    #[test]
    fn supplied_palette_ignores_generated_color_count() {
        let source = super::super::build_raster(
            super::super::Grid { width: 1, height: 1, geo_transform: [0., 1., 0., 1., 0., -1.] },
            vec![vec![0.], vec![0.], vec![0.]],
            wbraster::DataType::U8,
            None,
            &crate::crs::SpatialRef::default(),
            super::super::convert_value,
        )
        .unwrap();
        for count in [0, 1, 257, usize::MAX] {
            let options =
                PaletteOptions { color_count: count, color_table: Some(vec![[0, 0, 0, 255], [255, 255, 255, 255]]), ..Default::default() };
            assert!(rgb_to_palette(&source, &options).is_ok());
            assert!(rgb_to_palette(&source, &PaletteOptions { color_table: None, ..options.clone() }).is_err());
            assert!(rgb_to_palette(&source, &PaletteOptions { bit_depth: 6, ..options }).is_err());
        }
    }

    #[test]
    fn rgba_expansion_combines_dataset_validity_with_table_alpha() {
        let source = super::super::build_raster(
            super::super::Grid { width: 2, height: 1, geo_transform: [0., 1., 0., 1., 0., -1.] },
            vec![vec![0., 1.]],
            wbraster::DataType::U8,
            None,
            &crate::crs::SpatialRef::default(),
            super::super::convert_value,
        )
        .unwrap();
        let indexed = attach_palette(&source, &[[10, 20, 30, 255], [40, 50, 60, 128]]).unwrap();
        for alpha in [false, true] {
            let mut source = crate::LazyRaster::from_masked_dataset(indexed.clone(), vec![0, 255]).unwrap();
            let mut output = palette_to_rgb_lazy(&mut source, alpha).unwrap();
            let values = output.read().unwrap().raster.data.to_f64_vec();
            let mut expected = vec![10., 40., 20., 50., 30., 60.];
            if alpha {
                expected.extend([0., 128.]);
            }
            assert_eq!(values, expected);
            assert_eq!(output.read_mask(crate::Window::new(0, 0, 2, 1), None).unwrap(), Some(vec![0, 255]));
        }
    }

    #[test]
    fn palette_expansion_rejects_inconsistent_storage_instead_of_panicking() {
        let source = super::super::build_raster(
            super::super::Grid { width: 2, height: 1, geo_transform: [0., 1., 0., 1., 0., -1.] },
            vec![vec![0., 0.]],
            wbraster::DataType::U8,
            None,
            &crate::crs::SpatialRef::default(),
            super::super::convert_value,
        )
        .unwrap();
        let mut source = attach_palette(&source, &[[10, 20, 30, 255]]).unwrap();
        source.raster.data = RasterData::U8(vec![0]);
        assert!(palette_to_rgb(&source, true).unwrap_err().to_string().contains("one value per pixel"));
        source.raster.rows = usize::MAX;
        source.raster.cols = 2;
        assert!(palette_to_rgb(&source, true).unwrap_err().to_string().contains("overflow"));
    }

    #[test]
    fn median_cut_and_diffusion_match_independent_gdal_3133_fixture() {
        // Three bands of a GDAL-written 3x2 TIFF; reference outputs come from
        // `gdal raster rgb-to-palette --color-count 3 --bit-depth 5|8`.
        let red = [0, 32, 64, 255, 240, 12];
        let green = [2, 70, 128, 250, 7, 200];
        let blue = [40, 12, 255, 100, 10, 180];
        let rgb: Vec<_> = (0..6).map(|i| [red[i], green[i], blue[i]]).collect();
        for bits in [5, 8] {
            let table = median_cut(&rgb, &[true; 6], 3, bits);
            let expected = if bits == 5 {
                vec![[240, 0, 8, 255], [4, 100, 108, 255], [140, 156, 128, 255]]
            } else {
                vec![[240, 7, 10, 255], [6, 101, 110, 255], [143, 160, 133, 255]]
            };
            assert_eq!(table, expected);
            for dither in [true, false] {
                let output =
                    map_pixels(&rgb, &[true; 6], 3, &table, &PaletteOptions { bit_depth: bits, dither, ..Default::default() }, None, None);
                let expected = if bits == 5 && !dither { vec![1, 1, 2, 2, 0, 1] } else { vec![1, 1, 1, 2, 0, 1] };
                assert_eq!(output, expected, "bits={bits},dither={dither}");
            }
        }
    }
    #[test]
    fn shared_nodata_and_alpha_are_excluded_and_expand_transparently() {
        let mut source = super::super::build_raster_data(
            super::super::Grid { width: 3, height: 1, geo_transform: [0., 1., 0., 1., 0., -1.] },
            RasterData::U8(vec![0, 248, 248, 0, 0, 0, 0, 0, 0]),
            3,
            Some(0.),
            &Default::default(),
        )
        .unwrap();
        let indexed = rgb_to_palette(&source, &PaletteOptions { color_count: 3, ..Default::default() }).unwrap();
        assert_eq!(indexed.raster.data.to_f64_vec(), [0., 1., 1.]);
        assert_eq!(indexed.band_nodata(1), Some(0.));
        assert_eq!(indexed.color_table, [[0, 0, 0, 0], [248, 0, 0, 255]]);
        let expanded = palette_to_rgb(&indexed, true).unwrap();
        assert_eq!(expanded.raster.data.to_f64_vec(), [0., 248., 248., 0., 0., 0., 0., 0., 0., 0., 255., 255.]);
        assert!(!expanded.has_nodata);
        source.raster.data = RasterData::U8(vec![0, 248, 248, 0, 0, 0, 0, 0, 0, 0, 255, 255]);
        source.raster.bands = 4;
        source.has_nodata = false;
        source.band_metadata = ["Red", "Green", "Blue", "Alpha"]
            .iter()
            .map(|c| BandMetadata { color_interp: Some((*c).into()), ..Default::default() })
            .collect();
        let indexed = rgb_to_palette(&source, &Default::default()).unwrap();
        assert_eq!(indexed.raster.data.to_f64_vec(), [0., 1., 1.]);
        let attached = attach_palette(&indexed, &[[1, 2, 3, 4], [5, 6, 7, 8]]).unwrap();
        assert_eq!(attached.raster.data.to_f64_vec(), indexed.raster.data.to_f64_vec());
        assert_eq!(attached.band_nodata(1), Some(0.));
        assert_eq!(attached.color_table, [[1, 2, 3, 4], [5, 6, 7, 8]]);
    }
    #[test]
    fn indexed_color_files_preserve_sparse_indices_and_xml_alpha() {
        let text = parse_color_table("0 1 2 3 4\n2 red 128", "colors.txt").unwrap();
        assert_eq!(text, [[1, 2, 3, 4], [0, 0, 0, 0], [255, 0, 0, 128]]);
        let cpt = parse_color_table(
            "# COLOR_MODEL = RGB\n0 1 2 3 2 4 5 6\n2 7 8 9 4 10 11 12\nB 0 0 0\nF 255 255 255\nN 255 0 0\n",
            "colors.cpt",
        )
        .unwrap();
        assert_eq!(cpt, [[1, 2, 3, 255], [0; 4], [7, 8, 9, 255], [0; 4], [10, 11, 12, 255]]);
        let xml = r##"<qgis><pipe><rasterrenderer type="paletted"><colorPalette><paletteEntry value="2" color="#ff0000" alpha="128"/></colorPalette></rasterrenderer></pipe></qgis>"##;
        assert_eq!(parse_color_table(xml, "colors.qml").unwrap(), [[0, 0, 0, 0], [0, 0, 0, 0], [255, 0, 0, 128]]);
        for filename in ["colors.txt", "color-map", "colors.qml"] {
            assert_eq!(parse_color_table(&format!("\u{feff}{xml}"), filename).unwrap(), parse_color_table(xml, "colors.qml").unwrap());
        }
        assert!(parse_color_table("1.5 red", "colors.txt").is_err());
        assert!(parse_color_table("65536 red", "colors.txt").is_err());
        assert!(parse_color_table("50% red", "colors.txt").is_err());
        assert_eq!(parse_color_table("# Reflectance (%)\n/ 100% comment\n0 red", "colors.txt").unwrap(), [[255, 0, 0, 255]]);
        for text in ["0 256 0 0", "0 -1 0 0", "0 1 2 3 256", "0 1 2 3 -1"] {
            assert!(parse_color_table(text, "colors.txt").unwrap_err().to_string().contains("0..255"));
        }
        assert!(parse_color_table(&xml.replace("alpha=\"128\"", "alpha=\"256\""), "colors.qml").is_err());
    }
    #[test]
    fn gdal_nodata_precedence_and_diffused_nodata_fixture() {
        let rgb = [[0, 0, 0], [248, 248, 248], [248, 248, 248], [248, 248, 248]];
        let options = PaletteOptions { mask: Some(vec![255, 0, 255, 255]), ..Default::default() };
        let mut source = super::super::build_raster_data(
            super::super::Grid { width: 2, height: 2, geo_transform: [0., 1., 0., 2., 0., -1.] },
            RasterData::U8((0..3).flat_map(|b| rgb.iter().map(move |c| c[b])).collect()),
            3,
            Some(0.),
            &Default::default(),
        )
        .unwrap();
        // Independently generated with GDAL 3.13.3: shared nodata wins the mask.
        assert_eq!(rgb_to_palette(&source, &options).unwrap().raster.data.to_f64_vec(), [0., 1., 1., 1.]);
        source.raster.data = RasterData::U8(vec![1; 12]);
        let options = PaletteOptions { color_table: Some(vec![[4, 4, 4, 255]]), output_nodata: Some(0), ..Default::default() };
        // GDAL tests values after vertical diffusion: the second row becomes nodata.
        assert_eq!(rgb_to_palette(&source, &options).unwrap().raster.data.to_f64_vec(), [1., 1., 0., 0.]);
    }
    #[test]
    fn supplied_full_palette_nodata_insertion_matches_gdal() {
        let source = super::super::build_raster_data(
            super::super::Grid { width: 3, height: 1, geo_transform: [0., 1., 0., 1., 0., -1.] },
            RasterData::U8(vec![255, 254, 0, 255, 254, 0, 255, 254, 0]),
            3,
            None,
            &Default::default(),
        )
        .unwrap();
        let options = PaletteOptions {
            bit_depth: 8,
            dither: false,
            output_nodata: Some(0),
            color_table: Some((0..=255).map(|c| [c, c, c, 255]).collect()),
            ..Default::default()
        };
        let result = rgb_to_palette(&source, &options).unwrap();
        // GDAL 3.13.3 inserts the nodata slot and truncates the old last entry.
        assert_eq!(result.raster.data.to_f64_vec(), [255., 255., 1.]);
        assert_eq!(result.color_table.len(), 256);
        assert_eq!(result.color_table[0], [0, 0, 0, 0]);
        assert_eq!(result.color_table[255], [254, 254, 254, 255]);
    }
    #[test]
    fn high_nodata_padding_matches_gdal_virtual_palette() {
        let source = super::super::build_raster_data(
            super::super::Grid { width: 6, height: 1, geo_transform: [0., 1., 0., 1., 0., -1.] },
            RasterData::U8([1, 16, 32, 48, 64, 240].repeat(3)),
            3,
            None,
            &Default::default(),
        )
        .unwrap();
        let result =
            rgb_to_palette(&source, &PaletteOptions { color_count: 2, output_nodata: Some(200), dither: false, ..Default::default() })
                .unwrap();
        // GDAL 3.13.3 GDALG exposes transparent black padding and can select
        // its index 2 for a valid dark pixel. GeoTIFF normalizes non-nodata alpha.
        assert_eq!(result.raster.data.to_f64_vec(), [2., 0., 0., 0., 0., 1.]);
        assert_eq!(result.color_table[0], [8, 8, 8, 255]);
        assert_eq!(result.color_table[1], [136, 136, 136, 255]);
        assert_eq!(result.color_table[2], [0; 4]);
        assert_eq!(result.color_table[200], [0; 4]);
    }
    #[test]
    fn full_precision_retains_first_occurrence_and_nearest_ties() {
        let rgb = [[250, 1, 2], [4, 5, 6], [250, 1, 2]];
        assert_eq!(median_cut(&rgb, &[true; 3], 256, 8), [[250, 1, 2, 255], [4, 5, 6, 255]]);
        assert_eq!(nearest([10, 0, 0], &[[0, 0, 0, 255], [20, 0, 0, 255]], None), 0);
        assert_eq!(nearest([10, 0, 0], &[[0, 0, 0, 255], [20, 0, 0, 255]], Some(0)), 1);
    }
}
