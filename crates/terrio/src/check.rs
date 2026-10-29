//! Read every exposed sample or feature, reporting reader failures rather than
//! asserting checksum integrity. Successful decoding cannot detect every mutation.
use crate::{Error, LazyRaster, Result, Window, WindowOptions};

/// Optional auxiliary raster payloads beyond GDAL's default base-band check.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CheckOptions {
    /// Also read every exposed raster overview.
    pub overviews: bool,
    /// Also read per-dataset raster masks at each checked level.
    pub masks: bool,
}

/// Amount of content successfully decoded. A failure is returned as an error;
/// a successful summary is not a guarantee against undetectable corruption.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct CheckSummary {
    /// Raster sample values, summed over bands and requested levels.
    pub raster_samples: u64,
    /// Individual raster windows decoded.
    pub raster_windows: u64,
    /// Dataset-mask samples decoded when requested.
    pub mask_samples: u64,
    /// Vector layers checked, including empty layers.
    pub layers: u64,
    /// Vector features decoded.
    pub features: u64,
    /// Scientific numeric arrays checked, including coordinate arrays.
    pub arrays: u64,
    /// Scientific array elements decoded.
    pub array_elements: u64,
    /// Point records decoded in builds with point-cloud support.
    pub points: u64,
}
fn add(total: &mut u64, count: usize) -> Result<()> {
    *total = total.checked_add(count as u64).ok_or_else(|| Error::InvalidData("dataset check count overflows".into()))?;
    Ok(())
}

/// Reads all bands in two-dimensional windows of about 8 MiB, aligning to
/// source blocks when they fit. Oversized compressed blocks may be decoded again
/// for each window that intersects them. Codec working memory and readers without
/// lazy support retain their existing allocation behavior.
pub fn check_raster(source: &mut LazyRaster, options: &CheckOptions) -> Result<CheckSummary> {
    let mut summary = CheckSummary::default();
    let mut levels = vec![(None, source.width(), source.height())];
    if options.overviews {
        levels.extend(source.overviews().into_iter().enumerate().map(|(index, (width, height))| (Some(index), width, height)));
    }
    let pixel_bytes = source
        .band_count()
        .checked_mul(source.data_type().size_bytes())
        .filter(|bytes| *bytes > 0)
        .ok_or_else(|| Error::InvalidData("dataset check sample size is zero or overflows".into()))?;
    let budget = ((8 << 20) / pixel_bytes).max(1);
    for (overview, width, height) in levels {
        if width == 0 || height == 0 {
            continue;
        }
        let (block_width, block_height) = source.block_size().unwrap_or((1, 1));
        let bw = block_width.clamp(1, width);
        let bh = block_height.clamp(1, height);
        let (columns, rows) = if bw.checked_mul(bh).is_some_and(|area| area <= budget) {
            let columns = width.min((budget / bh / bw).max(1) * bw);
            (columns, height.min((budget / columns / bh).max(1) * bh))
        } else {
            // A huge strip/tile still needs its codec's working buffer, but do
            // not allocate an equally huge output window before that codec runs.
            let columns = width.min(bw.min(budget)).max(1);
            (columns, height.min((budget / columns).max(1)))
        };
        for y in (0..height).step_by(rows) {
            for x in (0..width).step_by(columns) {
                let window = Window::new(x, y, columns.min(width - x), rows.min(height - y));
                window
                    .width
                    .checked_mul(window.height)
                    .and_then(|pixels| pixels.checked_mul(pixel_bytes))
                    .ok_or_else(|| Error::InvalidData("dataset check window size overflows".into()))?;
                let data = source.read_window(window, &WindowOptions { overview, ..Default::default() })?;
                add(&mut summary.raster_samples, data.raster.data.len())?;
                add(&mut summary.raster_windows, 1)?;
                if options.masks
                    && let Some(mask) = source.read_mask(window, overview)?
                {
                    add(&mut summary.mask_samples, mask.len())?;
                }
            }
        }
    }
    Ok(summary)
}

fn array_chunks(array: &crate::multidim::Array, shape: &[usize]) -> Vec<usize> {
    if let Some(chunks) = &array.info.chunks
        && chunks.len() == shape.len()
        && !chunks.contains(&0)
    {
        let mut chunks: Vec<_> = chunks.iter().zip(shape).map(|(&chunk, &size)| chunk.min(size)).collect();
        let budget = (8 << 20) / array.info.data_type.size_bytes();
        if let Some(mut elements) = chunks.iter().try_fold(1usize, |n, &chunk| n.checked_mul(chunk))
            && elements <= budget
        {
            // Coalesce whole adjacent chunks so tiny storage chunks do not
            // require one hyperslab/view per chunk. Keep interior boundaries
            // aligned, leaving only the final slab clipped to the array edge.
            for axis in (0..shape.len()).rev() {
                let unit = chunks[axis];
                let others = elements / unit;
                chunks[axis] = (budget / others / unit).min(shape[axis] / unit) * unit;
                elements = others * chunks[axis];
            }
            return chunks;
        }
    }
    // Unknown or oversized storage chunks use bounded slabs. A backend may
    // decode the same storage chunk for multiple slabs in this case.
    crate::multidim::write::default_chunks(shape, array.info.data_type)
}

/// Reads all numeric arrays and their coordinates by bounded hyperslabs.
/// Unsupported array types fail explicitly instead of yielding a false success.
pub fn check_multidim(dataset: &crate::multidim::Dataset) -> Result<CheckSummary> {
    fn supported(group: &crate::multidim::Group) -> Result<()> {
        if let Some(array) = group.unsupported_arrays.first() {
            return Err(Error::Unsupported(format!("dataset check cannot decode array '{}' of type '{}'", array.path, array.data_type)));
        }
        for child in &group.groups {
            supported(child)?;
        }
        Ok(())
    }
    supported(&dataset.root)?;
    let mut summary = CheckSummary::default();
    for array in dataset.root.arrays_recursive() {
        add(&mut summary.arrays, 1)?;
        let shape = array.shape();
        if shape.contains(&0) {
            continue;
        }
        let chunks = array_chunks(array, &shape);
        let mut starts = vec![0usize; shape.len()];
        loop {
            let mut view = array.view();
            for axis in 0..shape.len() {
                view = view.slice_axis(axis, starts[axis], chunks[axis].min(shape[axis] - starts[axis]), 1)?;
            }
            let bytes = view.read()?.bytes.len();
            add(&mut summary.array_elements, bytes / array.info.data_type.size_bytes())?;
            let mut next = false;
            for axis in (0..shape.len()).rev() {
                let remaining = shape[axis] - starts[axis];
                if chunks[axis] < remaining {
                    starts[axis] += chunks[axis];
                    next = true;
                    break;
                }
                starts[axis] = 0;
            }
            if !next {
                break;
            }
        }
    }
    Ok(summary)
}

fn check_decoded(dataset: crate::Dataset, options: &CheckOptions) -> Result<CheckSummary> {
    match dataset {
        crate::Dataset::Raster(dataset) => check_raster(&mut dataset.into(), options),
        crate::Dataset::Vector(dataset) => {
            let mut summary = CheckSummary::default();
            for layer in dataset.layers {
                add(&mut summary.layers, 1)?;
                add(&mut summary.features, layer.features.len())?;
            }
            Ok(summary)
        }
    }
}

/// Reads all exposed content of a native dataset. Stream-capable vector readers
/// use batches; other readers may materialize. Scientific groups include all
/// numeric arrays rather than just the default raster variable.
#[cfg(not(target_family = "wasm"))]
pub fn check_path(path: &str, options: &CheckOptions) -> Result<CheckSummary> {
    use crate::formats::Kind;
    if ["NETCDF:", "HDF5:", "ZARR:", "GPKG:", "GDAL:"]
        .iter()
        .any(|prefix| path.get(..prefix.len()).is_some_and(|head| head.eq_ignore_ascii_case(prefix)))
    {
        return check_raster(&mut crate::open_raster_lazy(path)?, options);
    }
    let format = crate::inspection::identify(path, false)?;
    if format.as_ref().is_some_and(|format| format.multidimensional) {
        return check_multidim(&crate::multidim::open(path)?);
    }
    #[cfg(feature = "pointcloud")]
    if format.as_ref().is_some_and(|format| format.kinds.contains(&Kind::PointCloud)) || crate::pointcloud::is_point_cloud_path(path) {
        let mut reader = crate::pointcloud::open_points(path)?;
        let mut summary = CheckSummary::default();
        for chunk in reader.chunks(&Default::default(), 65_536)? {
            add(&mut summary.points, chunk?.len())?;
        }
        return Ok(summary);
    }
    #[cfg(feature = "sqlite")]
    if format.as_ref().is_some_and(|format| format.driver == "GPKG") {
        let local = crate::vfs::resolve(path)?;
        let db = crate::stream::sqlite::FileDb::open(local.as_str())?;
        let (layers, rasters) = crate::drivers::vector::gpkg_read::contents(&db)?;
        let mut summary = if layers.is_empty() { CheckSummary::default() } else { check_vector_path(local.as_str())? };
        for table in rasters {
            let name = format!("GPKG:{}:{}", local.as_str(), table.name);
            merge(&mut summary, check_raster(&mut crate::open_raster_lazy(&name)?, options)?)?;
        }
        return Ok(summary);
    }
    #[cfg(feature = "tiles")]
    if format.as_ref().is_some_and(|format| matches!(format.driver.as_str(), "PMTiles" | "MBTiles")) {
        let archive = crate::tiles::TileArchive::open(path)?;
        if archive.is_vector() {
            return check_decoded(crate::tiles::archive_dataset(archive, path, &Default::default())?, options);
        }
        return check_raster(&mut LazyRaster::from_tile_archive(archive)?, options);
    }
    if let Some(format) = format {
        if format.kinds == [Kind::Vector] {
            return check_vector_path(path);
        }
        return check_raster(&mut crate::open_raster_lazy(path)?, options);
    }
    // Scientific signatures can lie beyond the identification prefix (HDF5
    // user blocks). A full check must still include every numeric array.
    let scientific_error = match crate::multidim::open(path) {
        Ok(dataset) => return check_multidim(&dataset),
        Err(error) => error,
    };
    // Companion-file formats and reader-specific dataset names need the normal
    // dispatch fallback when the bounded identifier has no signature.
    let raster_error = match crate::open_raster_lazy(path) {
        Ok(mut raster) => return check_raster(&mut raster, options),
        Err(error) => error,
    };
    // Reader error variants do not reliably distinguish an unrecognized input
    // from corrupt content. Preserve every failed probe rather than guessing
    // which one describes the input's actual format.
    check_vector_path(path).map_err(|vector_error| {
        Error::InvalidData(format!(
            "dataset check could not open unrecognized input; multidimensional reader: {scientific_error}; raster reader: {raster_error}; vector reader: {vector_error}"
        ))
    })
}

#[cfg(not(target_family = "wasm"))]
fn check_vector_path(path: &str) -> Result<CheckSummary> {
    let mut summary = CheckSummary::default();
    for mut reader in crate::stream::open_layers(path, &Default::default(), 4096)? {
        add(&mut summary.layers, 1)?;
        while let Some(chunk) = reader.read_chunk(4096)? {
            add(&mut summary.features, chunk.features.len())?;
        }
    }
    Ok(summary)
}

#[cfg(feature = "sqlite")]
fn merge(summary: &mut CheckSummary, other: CheckSummary) -> Result<()> {
    macro_rules! sum {
        ($($field:ident),*) => { $(summary.$field = summary.$field.checked_add(other.$field)
            .ok_or_else(|| Error::InvalidData("dataset check count overflows".into()))?;)* };
    }
    sum!(raster_samples, raster_windows, mask_samples, layers, features, arrays, array_elements, points);
    Ok(())
}

fn has_hdf5_signature(bytes: &[u8]) -> bool {
    if bytes.len() < 8 {
        return false;
    }
    let mut offset = 0usize;
    while offset <= bytes.len() - 8 {
        if &bytes[offset..offset + 8] == b"\x89HDF\r\n\x1a\n" {
            return true;
        }
        let Some(next) = (if offset == 0 { Some(512) } else { offset.checked_mul(2) }) else {
            break;
        };
        offset = next;
    }
    false
}

/// Checks uploaded content. TIFF decoding stays windowed; other formats use the
/// existing in-memory readers. Scientific uploads include all numeric arrays.
pub fn check_bytes(bytes: Vec<u8>, filename: &str, options: &CheckOptions) -> Result<CheckSummary> {
    let format = crate::inspection::identify_bytes(&bytes, filename);
    if format.as_ref().is_some_and(|format| format.multidimensional) || (format.is_none() && has_hdf5_signature(&bytes)) {
        return check_multidim(&crate::multidim::open_bytes(&bytes)?);
    }
    #[cfg(feature = "pointcloud")]
    if format.as_ref().is_some_and(|format| format.kinds.contains(&crate::formats::Kind::PointCloud)) {
        let mut reader = crate::pointcloud::open_points_bytes(bytes, filename)?;
        let mut summary = CheckSummary::default();
        for chunk in reader.chunks(&Default::default(), 65_536)? {
            add(&mut summary.points, chunk?.len())?;
        }
        return Ok(summary);
    }
    #[cfg(feature = "sqlite")]
    if format.as_ref().is_some_and(|format| format.driver == "GPKG") {
        let db = crate::drivers::sqlite::Db::new(bytes)?;
        let (_, rasters) = crate::drivers::vector::gpkg_read::contents(&db)?;
        let mut summary = CheckSummary::default();
        for layer in crate::drivers::vector::gpkg_read::read_db(&db, &[], None)? {
            add(&mut summary.layers, 1)?;
            add(&mut summary.features, layer.features.len())?;
        }
        for raster in rasters {
            let source = crate::drivers::gpkg::read_db(&db, Some(&raster.name), std::path::Path::new(filename))?;
            merge(&mut summary, check_raster(&mut source.into(), options)?)?;
        }
        return Ok(summary);
    }
    if format.as_ref().is_some_and(|format| format.driver == "GTiff") {
        return check_raster(&mut crate::memory::open_raster_lazy_bytes(bytes, filename)?, options);
    }
    check_decoded(crate::memory::open_bytes(bytes, filename)?, options)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DataType, Raster, RasterConfig, RasterDataset};
    use wbraster::raster::RasterData;

    fn raster(width: usize, height: usize, bands: usize) -> RasterDataset {
        let config = RasterConfig { cols: width, rows: height, bands, data_type: DataType::U8, ..Default::default() };
        RasterDataset::new(Raster::from_data_native(config, RasterData::U8(vec![1; width * height * bands])).unwrap(), false)
    }

    #[test]
    fn all_bands_levels_and_optional_masks_are_read_in_bounded_windows() {
        struct Source(bool);
        impl crate::cog::WindowSource for Source {
            fn overviews(&self) -> Vec<(usize, usize)> {
                vec![(512, 512)]
            }
            fn block_size(&self) -> Option<(usize, usize)> {
                Some((256, 256))
            }
            fn has_mask(&self) -> bool {
                true
            }
            fn read_mask(&mut self, _: usize, window: Window) -> Result<Option<Vec<u8>>> {
                Ok(Some(vec![255; window.width * window.height]))
            }
            fn read(&mut self, level: usize, window: Window, bands: &[usize]) -> Result<RasterData> {
                assert_eq!(bands, [1, 2, 3]);
                assert!(window.height <= 1280);
                if self.0 && (window.y > 0 || level > 0) {
                    return Err(Error::InvalidData("late chunk decode failure".into()));
                }
                Ok(RasterData::U8(vec![1; window.width * window.height * bands.len()]))
            }
        }
        let mut header = raster(1, 1, 3);
        header.raster.cols = 2048;
        header.raster.rows = 2048;
        header.raster.data = RasterData::U8(Vec::new());
        let mut source = LazyRaster::from_source(header.clone(), Box::new(Source(false)));
        let base = check_raster(&mut source, &Default::default()).unwrap();
        assert_eq!(base.raster_samples, 2048 * 2048 * 3);
        assert_eq!(base.raster_windows, 2);
        assert_eq!(base.mask_samples, 0);
        let all = check_raster(&mut source, &CheckOptions { overviews: true, masks: true }).unwrap();
        assert_eq!(all.raster_samples, (2048 * 2048 + 512 * 512) * 3);
        assert_eq!(all.mask_samples, 2048 * 2048 + 512 * 512);
        assert_eq!(all.raster_windows, 3);
        let mut bad = LazyRaster::from_source(header, Box::new(Source(true)));
        assert!(check_raster(&mut bad, &Default::default()).unwrap_err().to_string().contains("late chunk"));
    }

    #[test]
    fn very_wide_rasters_do_not_allocate_a_full_width_tile_row() {
        struct Wide;
        impl crate::cog::WindowSource for Wide {
            fn overviews(&self) -> Vec<(usize, usize)> {
                Vec::new()
            }
            fn block_size(&self) -> Option<(usize, usize)> {
                Some((512, 512))
            }
            fn read(&mut self, _: usize, window: Window, bands: &[usize]) -> Result<RasterData> {
                assert!(window.width < 1_000_000);
                assert!(window.width * window.height * bands.len() <= 8 << 20);
                assert_eq!(window.width % 512, 0);
                assert_eq!(window.height % 512, 0);
                Err(Error::InvalidData("stop after inspecting the first planned window".into()))
            }
        }
        let mut header = raster(1, 1, 3);
        header.raster.cols = 1_000_000;
        header.raster.rows = 1024;
        header.raster.data = RasterData::U8(Vec::new());
        let mut source = LazyRaster::from_source(header, Box::new(Wide));
        assert!(check_raster(&mut source, &Default::default()).unwrap_err().to_string().contains("first planned window"));
    }

    #[test]
    #[cfg(not(target_family = "wasm"))]
    fn fallback_failures_keep_all_reader_diagnostics() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("unknown.bin");
        std::fs::write(&path, b"not a dataset").unwrap();
        let path = path.to_str().unwrap();
        let scientific = crate::multidim::open(path).unwrap_err().to_string();
        let raster = crate::open_raster_lazy(path).err().unwrap().to_string();
        let vector = check_vector_path(path).unwrap_err().to_string();
        let message = check_path(path, &Default::default()).unwrap_err().to_string();
        assert!(message.contains(&format!("multidimensional reader: {scientific}")));
        assert!(message.contains(&format!("raster reader: {raster}")));
        assert!(message.contains(&format!("vector reader: {vector}")));
    }

    #[test]
    fn oversized_strips_bound_output_but_repeat_block_decoding() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        struct Strips(Arc<AtomicUsize>);
        impl crate::cog::WindowSource for Strips {
            fn overviews(&self) -> Vec<(usize, usize)> {
                Vec::new()
            }
            fn block_size(&self) -> Option<(usize, usize)> {
                Some((1_000_000, 16))
            }
            fn read(&mut self, _: usize, window: Window, bands: &[usize]) -> Result<RasterData> {
                assert!(window.width * window.height * bands.len() <= 8 << 20);
                let first = window.y / 16;
                let last = (window.y + window.height - 1) / 16;
                self.0.fetch_add(last - first + 1, Ordering::Relaxed);
                Ok(RasterData::U8(vec![1; window.width * window.height * bands.len()]))
            }
        }
        let decoded = Arc::new(AtomicUsize::new(0));
        let mut header = raster(1, 1, 1);
        header.raster.cols = 1_000_000;
        header.raster.rows = 16;
        header.raster.data = RasterData::U8(Vec::new());
        let mut source = LazyRaster::from_source(header, Box::new(Strips(decoded.clone())));
        let summary = check_raster(&mut source, &Default::default()).unwrap();
        assert_eq!(summary.raster_samples, 16_000_000);
        assert_eq!(summary.raster_windows, 2);
        assert_eq!(decoded.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn scientific_scalar_empty_and_multichunk_arrays_are_checked() {
        use crate::multidim::{Array, ArrayInfo, Dataset, Dimension, Group, UnsupportedArray};
        let mut root = Group::new("/");
        for (name, shape) in [("scalar", vec![]), ("empty", vec![0]), ("grid", vec![300, 400])] {
            let dimensions = shape
                .iter()
                .enumerate()
                .map(|(axis, &size)| Dimension {
                    name: format!("d{axis}"),
                    path: format!("/d{axis}"),
                    size,
                    unlimited: false,
                    indexing_variable: None,
                })
                .collect();
            let info = ArrayInfo {
                name: name.into(),
                path: format!("/{name}"),
                data_type: DataType::U8,
                dimensions,
                chunks: None,
                attributes: serde_json::json!({}),
                coordinates: Vec::new(),
            };
            root.arrays.push(Array::from_bytes(info, vec![1; shape.iter().product()]).unwrap());
        }
        let mut dataset = Dataset { driver: "Memory".into(), root };
        let summary = check_multidim(&dataset).unwrap();
        assert_eq!(summary.arrays, 3);
        assert_eq!(summary.array_elements, 120_001);
        dataset.root.unsupported_arrays.push(UnsupportedArray { path: "/text".into(), data_type: "string".into(), shape: vec![2] });
        assert!(check_multidim(&dataset).unwrap_err().to_string().contains("cannot decode array '/text'"));
    }

    #[test]
    fn malformed_uploaded_vector_does_not_pass_as_an_empty_dataset() {
        let source = br#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{},"geometry":{"type":"Point","coordinates":[0,0]}}]}"#;
        let summary = check_bytes(source.to_vec(), "points.geojson", &Default::default()).unwrap();
        assert_eq!((summary.layers, summary.features), (1, 1));
        assert!(check_bytes(source[..source.len() - 4].to_vec(), "points.geojson", &Default::default()).is_err());
    }

    #[test]
    #[cfg(feature = "sqlite")]
    fn uploaded_geopackage_checks_vector_layers_and_every_raster_table() {
        let source = raster(3, 2, 1);
        let mut builder = crate::drivers::vector::geopackage::Builder::new();
        let mut layer = crate::Layer::new("points");
        layer.add_feature(Some(wbvector::Geometry::point(1., 2.)), &[]).unwrap();
        builder.add_layer(&layer).unwrap();
        for name in ["first", "second"] {
            crate::drivers::gpkg::add_raster(&mut builder, &source.raster, None, name, None).unwrap();
        }
        let bytes = builder.finish().unwrap();
        let summary = check_bytes(bytes.clone(), "mixed.gpkg", &Default::default()).unwrap();
        assert_eq!((summary.layers, summary.features), (1, 1));
        assert_eq!(summary.raster_samples, 12);
        #[cfg(not(target_family = "wasm"))]
        {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("mixed.gpkg");
            std::fs::write(&path, bytes).unwrap();
            let native = check_path(path.to_str().unwrap(), &Default::default()).unwrap();
            assert_eq!((native.raster_samples, native.layers, native.features), (12, 1, 1));
        }
    }
}
