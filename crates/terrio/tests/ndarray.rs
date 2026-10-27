//! The `ndarray` feature (`cargo test -p terrio --features ndarray --test ndarray`): raster pixels
//! as arrays, without copying them. Views and moves are checked against the raster's buffer (its
//! pointer, capacity and values) and by what they allocate, counted by this test's allocator.
#![cfg(feature = "ndarray")]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use rayon::prelude::*;
use terrio::formats::RasterDriver;
use terrio::ndarray::{Array2, Array3, ArrayViewMut2, Axis, ShapeBuilder, arr2, s};
use terrio::raster::{CrsInfo, RasterData};
use terrio::{
    BandMetadata, DataType, Error, Gcp, Geolocation, LazyRaster, Raster, RasterArrayExt, RasterDataset, RasterElement, RasterWriteOptions,
    Window, WindowOptions,
};

/// Allocations, reallocations and releases of memory, and the bytes allocated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Counts {
    allocs: usize,
    reallocs: usize,
    frees: usize,
    bytes: usize,
}

const NONE: Counts = Counts { allocs: 0, reallocs: 0, frees: 0, bytes: 0 };

thread_local! {
    /// What this thread has allocated: each test runs on a thread of its own.
    static COUNTS: Cell<Counts> = const { Cell::new(NONE) };
}

fn record(f: impl FnOnce(&mut Counts)) {
    // The counts of a thread being torn down may be gone.
    let _ = COUNTS.try_with(|c| {
        let mut counts = c.get();
        f(&mut counts);
        c.set(counts);
    });
}

/// The system allocator, counting what each thread allocates.
struct Counting;

// SAFETY: every call goes to the system allocator, unchanged.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record(|c| {
            c.allocs += 1;
            c.bytes += layout.size();
        });
        // SAFETY: as the caller guarantees.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record(|c| {
            c.allocs += 1;
            c.bytes += layout.size();
        });
        // SAFETY: as the caller guarantees.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        record(|c| {
            c.reallocs += 1;
            c.bytes += new_size;
        });
        // SAFETY: as the caller guarantees.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        record(|c| c.frees += 1);
        // SAFETY: as the caller guarantees.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// The result of `f`, with what it allocated on this thread.
fn counted<R>(f: impl FnOnce() -> R) -> (R, Counts) {
    let before = COUNTS.with(Cell::get);
    let result = f();
    let after = COUNTS.with(Cell::get);
    let counts = Counts {
        allocs: after.allocs - before.allocs,
        reallocs: after.reallocs - before.reallocs,
        frees: after.frees - before.frees,
        bytes: after.bytes - before.bytes,
    };
    (result, counts)
}

const ROWS: usize = 40;
const COLS: usize = 50;
const BAND: usize = ROWS * COLS;

/// `bands` bands of `ROWS` x `COLS` float pixels, each holding its index in the buffer but one
/// nodata pixel, in a buffer with room for `spare` more.
fn raster(bands: usize, spare: usize) -> Raster {
    let mut values = Vec::with_capacity(bands * BAND + spare);
    values.extend((0..bands * BAND).map(|i| i as f32));
    if let Some(v) = values.get_mut(7) {
        *v = -9999.0;
    }
    Raster {
        cols: COLS,
        rows: ROWS,
        bands,
        x_min: 500_000.0,
        y_min: 4_100_000.0,
        cell_size_x: 30.0,
        cell_size_y: 30.0,
        nodata: -9999.0,
        data_type: DataType::F32,
        crs: CrsInfo::from_epsg(32613),
        metadata: vec![("AREA_OR_POINT".into(), "Point".into())],
        data: RasterData::F32(values),
    }
}

/// [`raster`] with metadata in every field of the dataset.
fn dataset(bands: usize, spare: usize) -> RasterDataset {
    let mut ds = RasterDataset::with_driver(raster(bands, spare), RasterDriver::Cog, true);
    ds.set_geo_transform([500_000.0, 30.0, 1.5, 4_101_200.0, 1.5, -30.0]);
    ds.gcps = vec![Gcp { id: "1".into(), pixel: 0.5, line: 0.5, x: 500_015.0, y: 4_101_185.0, ..Default::default() }];
    ds.band_nodata = (0..bands).map(|b| (b != 1).then_some(-9999.0)).collect();
    ds.band_metadata = (0..bands)
        .map(|b| BandMetadata { description: Some(format!("band {b}")), scale: Some(0.5), unit: Some("m".into()), ..Default::default() })
        .collect();
    ds.geolocation = Some(Geolocation {
        srs: "EPSG:4326".into(),
        x_dataset: "lon.tif".into(),
        x_band: 1,
        y_dataset: "lat.tif".into(),
        y_band: 1,
        pixel_offset: 0.0,
        pixel_step: 1.0,
        line_offset: 0.0,
        line_step: 1.0,
        pixel_center: true,
    });
    ds
}

fn f32s(data: &RasterData) -> &[f32] {
    data.as_f32_slice().unwrap()
}

fn capacity(data: &RasterData) -> usize {
    let RasterData::F32(values) = data else { panic!("not Float32") };
    values.capacity()
}

#[test]
fn views_borrow_the_buffer() {
    let raster = raster(3, 0);
    let base = f32s(&raster.data).as_ptr();
    let (pixels, counts) = counted(|| raster.view3::<f32>().unwrap());
    assert_eq!(counts, NONE);
    assert_eq!(pixels.dim(), (3, ROWS, COLS));
    assert_eq!(pixels.strides(), &[BAND as isize, COLS as isize, 1]);
    assert!(pixels.is_standard_layout());
    assert_eq!(pixels.as_ptr(), base);
    assert_eq!(pixels[[1, 2, 3]], (BAND + 2 * COLS + 3) as f32);
    assert_eq!(pixels[[2, ROWS - 1, COLS - 1]], (3 * BAND - 1) as f32);
    assert_eq!(pixels[[0, 0, 7]], -9999.0); // nodata, as stored
    for band in 0..3 {
        let (view, counts) = counted(|| raster.band_view2::<f32>(band).unwrap());
        assert_eq!(counts, NONE);
        assert_eq!(view.dim(), (ROWS, COLS));
        assert_eq!(view.strides(), &[COLS as isize, 1]);
        assert_eq!(view.as_ptr(), base.wrapping_add(band * BAND));
        assert_eq!(view, pixels.index_axis(Axis(0), band));
    }
}

#[test]
fn mutable_views_write_to_the_raster() {
    let mut raster = raster(3, 0);
    let base = f32s(&raster.data).as_ptr();
    let (mut pixels, counts) = counted(|| raster.view3_mut::<f32>().unwrap());
    assert_eq!(counts, NONE);
    assert_eq!(pixels.as_ptr(), base);
    pixels[[1, 2, 3]] = 0.25;
    let (mut band, counts) = counted(|| raster.band_view2_mut::<f32>(2).unwrap());
    assert_eq!(counts, NONE);
    assert_eq!(band.as_ptr(), base.wrapping_add(2 * BAND));
    band.fill(1.5);
    let values = f32s(&raster.data);
    assert_eq!(values[BAND + 2 * COLS + 3], 0.25);
    assert!(values[2 * BAND..].iter().all(|v| *v == 1.5));
    assert_eq!(values[BAND - 1], (BAND - 1) as f32);
    assert_eq!(values.as_ptr(), base);
}

#[test]
fn bands_split_for_threads() {
    let mut raster = raster(3, 0);
    let base = f32s(&raster.data).as_ptr();
    let (bands, counts) = counted(|| raster.split_bands_mut::<f32>().unwrap());
    // One allocation, the vector of views: no pixels.
    let views = 3 * std::mem::size_of::<ArrayViewMut2<'_, f32>>();
    assert_eq!(counts, Counts { allocs: 1, reallocs: 0, frees: 0, bytes: views });
    for (b, band) in bands.iter().enumerate() {
        assert_eq!(band.dim(), (ROWS, COLS));
        assert_eq!(band.as_ptr(), base.wrapping_add(b * BAND));
    }
    bands.into_par_iter().enumerate().for_each(|(b, mut band)| band.mapv_inplace(|v| v + 10_000.0 * b as f32));
    for (i, v) in f32s(&raster.data).iter().enumerate() {
        let stored = if i == 7 { -9999.0 } else { i as f32 };
        assert_eq!(*v, stored + 10_000.0 * (i / BAND) as f32);
    }
}

#[test]
fn views_check_the_type_and_the_shape() {
    let mut raster = raster(3, 0);
    let error = raster.view3::<f64>().unwrap_err();
    assert!(matches!(&error, Error::InvalidArgument(m) if m == "the raster's pixels are Float32, not Float64"), "{error}");
    assert!(raster.view3_mut::<u8>().is_err());
    assert!(raster.band_view2::<i16>(0).is_err());
    assert!(raster.split_bands_mut::<u16>().is_err());
    let error = raster.band_view2::<f32>(3).unwrap_err();
    assert!(matches!(&error, Error::InvalidArgument(m) if m == "no band 3: the raster has 3 band(s), numbered from 0"), "{error}");
    assert!(raster.band_view2_mut::<f32>(3).is_err());
    // Fields that do not match the buffer.
    raster.cols += 1;
    let error = raster.view3::<f32>().unwrap_err();
    assert!(matches!(&error, Error::InvalidData(m) if m == "the raster holds 6000 values for 3 band(s) of 51 x 40 pixels"), "{error}");
    assert!(raster.view3_mut::<f32>().is_err());
    raster.cols = usize::MAX;
    assert!(matches!(raster.view3::<f32>(), Err(Error::InvalidData(_))));
    // No pixels, of a shape ndarray does not index (more than `isize::MAX` of them but for rows).
    raster.data = RasterData::F32(Vec::new());
    raster.rows = 0;
    assert!(matches!(raster.view3::<f32>(), Err(Error::InvalidData(_))));
    assert!(matches!(RasterDataset::new(raster.clone(), false).into_array3::<f32>(), Err(e) if matches!(e.error(), Error::InvalidData(_))));
    // No bands.
    (raster.bands, raster.rows, raster.cols) = (0, 2, 3);
    assert_eq!(raster.view3::<f32>().unwrap().dim(), (0, 2, 3));
    assert!(raster.split_bands_mut::<f32>().unwrap().is_empty());
}

#[test]
fn every_pixel_type() {
    fn check<T: RasterElement + PartialEq + std::fmt::Debug>(data: RasterData, values: [T; 2]) {
        assert_eq!(data.data_type(), T::DATA_TYPE);
        let raster = Raster { cols: 2, rows: 1, bands: 1, data_type: T::DATA_TYPE, data, ..raster(0, 0) };
        let ptr = raster.view3::<T>().unwrap().as_ptr();
        assert_eq!(raster.band_view2::<T>(0).unwrap(), arr2(&[values]));
        let (array, header) = RasterDataset::new(raster, false).into_array2::<T>().unwrap();
        assert_eq!((array.as_ptr(), array.dim()), (ptr, (1, 2)));
        let back = RasterDataset::from_array2(header, array).unwrap();
        assert_eq!(back.raster.view3::<T>().unwrap().as_ptr(), ptr);
    }
    check(RasterData::U8(vec![1, u8::MAX]), [1, u8::MAX]);
    check(RasterData::I8(vec![-1, i8::MAX]), [-1, i8::MAX]);
    check(RasterData::U16(vec![1, u16::MAX]), [1, u16::MAX]);
    check(RasterData::I16(vec![-1, i16::MAX]), [-1, i16::MAX]);
    check(RasterData::U32(vec![1, u32::MAX]), [1, u32::MAX]);
    check(RasterData::I32(vec![-1, i32::MAX]), [-1, i32::MAX]);
    check(RasterData::U64(vec![1, u64::MAX]), [1, u64::MAX]);
    check(RasterData::I64(vec![-1, i64::MAX]), [-1, i64::MAX]);
    check(RasterData::F32(vec![0.5, f32::MAX]), [0.5, f32::MAX]);
    check(RasterData::F64(vec![-0.5, f64::MAX]), [-0.5, f64::MAX]);
}

#[test]
fn datasets_move_into_arrays_and_back() {
    let ds = dataset(3, 16);
    let mut expected = ds.clone();
    let (ptr, room) = (f32s(&ds.raster.data).as_ptr(), capacity(&ds.raster.data));
    assert_eq!(room, 3 * BAND + 16);
    let ((array, header), counts) = counted(|| ds.into_array3::<f32>().unwrap());
    assert_eq!(counts, NONE);
    assert_eq!(array.dim(), (3, ROWS, COLS));
    assert!(array.is_standard_layout());
    assert_eq!(array.as_ptr(), ptr);
    assert_eq!(array[[0, 0, 7]], -9999.0);
    // The header is the dataset without pixels: an empty buffer of the same type, and every other
    // field as it was.
    let pixels = std::mem::replace(&mut expected.raster.data, RasterData::F32(Vec::new()));
    assert_eq!(format!("{header:?}"), format!("{expected:?}"));
    assert_eq!(header.band_nodata(1), Some(-9999.0));
    assert_eq!(header.band_nodata(2), None);
    // The array's buffer is the raster's, capacity included.
    let (values, offset) = array.into_raw_vec_and_offset();
    assert_eq!((values.as_ptr(), values.len(), values.capacity(), offset), (ptr, 3 * BAND, room, Some(0)));
    let array = Array3::from_shape_vec((3, ROWS, COLS), values).unwrap();
    // And back.
    let (back, counts) = counted(|| RasterDataset::from_array3(header, array).unwrap());
    assert_eq!(counts, NONE);
    expected.raster.data = pixels;
    assert_eq!(format!("{back:?}"), format!("{expected:?}"));
    assert_eq!((f32s(&back.raster.data).as_ptr(), capacity(&back.raster.data)), (ptr, room));
}

#[test]
fn single_bands_move_into_two_dimensional_arrays() {
    let ds = dataset(1, 0);
    let ptr = f32s(&ds.raster.data).as_ptr();
    let ((array, header), counts) = counted(|| ds.into_array2::<f32>().unwrap());
    assert_eq!(counts, NONE);
    assert_eq!((array.as_ptr(), array.dim(), array.strides()), (ptr, (ROWS, COLS), &[COLS as isize, 1][..]));
    let (back, counts) = counted(|| RasterDataset::from_array2(header, array).unwrap());
    assert_eq!(counts, NONE);
    assert_eq!(f32s(&back.raster.data).as_ptr(), ptr);
    // Three bands are not one, nor are a complex band's two parts.
    let error = dataset(3, 0).into_array2::<f32>().unwrap_err();
    assert_eq!(error.to_string(), "the raster has 3 bands, not one");
    assert_eq!(error.into_input().raster.bands, 3);
    let (array, header) = dataset(3, 0).into_array3::<f32>().unwrap();
    let (header, _) = RasterDataset::from_array2(header, array.index_axis_move(Axis(0), 0).to_owned()).unwrap_err().into_input();
    assert_eq!(header.raster.bands, 3);
}

#[test]
fn failed_moves_give_the_input_back() {
    // The dataset, as it was, when the type is not the pixels' or the fields not the buffer's.
    let ds = dataset(3, 0);
    let ptr = f32s(&ds.raster.data).as_ptr();
    let error = ds.into_array3::<f64>().unwrap_err();
    assert!(matches!(error.error(), Error::InvalidArgument(_)));
    assert!(format!("{error:?}").len() < 200, "the pixels are not printed: {error:?}");
    let mut ds = error.into_input();
    assert_eq!(f32s(&ds.raster.data).as_ptr(), ptr);
    ds.raster.rows -= 1;
    let (error, mut ds) = ds.into_array3::<f32>().unwrap_err().into_parts();
    assert!(matches!(error, Error::InvalidData(_)));
    ds.raster.rows += 1;
    assert_eq!(f32s(&ds.raster.data).as_ptr(), ptr);
    // `?` returns the error alone.
    fn doubles(ds: RasterDataset) -> terrio::Result<Array3<f64>> {
        Ok(ds.into_array3::<f64>()?.0)
    }
    assert!(matches!(doubles(ds.clone()), Err(Error::InvalidArgument(_))));

    // The header and the array, as they were, when shapes or types differ.
    let (array, header) = ds.into_array3::<f32>().unwrap();
    let mut other = header.clone();
    other.raster.cols -= 1;
    let error = RasterDataset::from_array3(other, array).unwrap_err();
    assert_eq!(error.to_string(), "the array's 3 band(s) of 50 x 40 pixels are not the header's 3 band(s) of 49 x 40");
    let (_, array) = error.into_input();
    let mut other = header.clone();
    other.raster.data_type = DataType::F64;
    let error = RasterDataset::from_array3(other, array).unwrap_err();
    assert_eq!(error.to_string(), "the array's Float32 values are not the header's Float64 pixels");
    let (_, array) = error.into_input();
    assert_eq!(array.as_ptr(), ptr);

    // An array whose elements would be reordered: given back untouched.
    let transposed = array.reversed_axes();
    let mut other = header.clone();
    (other.raster.bands, other.raster.rows, other.raster.cols) = (COLS, ROWS, 3);
    let error = RasterDataset::from_array3(other, transposed).unwrap_err();
    assert!(error.to_string().starts_with("the array is not in standard (row-major) layout"), "{error}");
    let (_, transposed) = error.into_input();
    assert_eq!((transposed.as_ptr(), transposed.strides()), (ptr, &[1, COLS as isize, BAND as isize][..]));
    let array = transposed.reversed_axes();

    // An array that is a part of its buffer: given back on the same buffer, at the same place.
    let mut two = header.clone();
    two.raster.bands = 2;
    let tail = array.slice_move(s![1.., .., ..]);
    let error = RasterDataset::from_array3(two.clone(), tail).unwrap_err();
    assert_eq!(error.to_string(), "the array is 4000 of the 6000 elements of its buffer, from element 2000: moving it would copy them");
    let (_, tail) = error.into_input();
    assert_eq!((tail.as_ptr(), tail.dim(), tail[[0, 0, 0]]), (ptr.wrapping_add(BAND), (2, ROWS, COLS), BAND as f32));
    assert_eq!(tail.strides(), &[BAND as isize, COLS as isize, 1]);
    let (values, offset) = tail.into_raw_vec_and_offset();
    assert_eq!((values.as_ptr(), values.len(), offset), (ptr, 3 * BAND, Some(BAND)));
    let head = Array3::from_shape_vec((3, ROWS, COLS), values).unwrap().slice_move(s![..2, .., ..]);
    let error = RasterDataset::from_array3(two, head).unwrap_err();
    assert!(error.to_string().starts_with("the array is 4000 of the 6000 elements of its buffer, from element 0"), "{error}");
    let (_, head) = error.into_input();
    assert_eq!((head.as_ptr(), head.dim(), head[[1, ROWS - 1, COLS - 1]]), (ptr, (2, ROWS, COLS), (2 * BAND - 1) as f32));
    assert_eq!(head.strides(), &[BAND as isize, COLS as isize, 1]);
}

/// A header without pixels for an array of `(bands, rows, cols)` float pixels.
fn header((bands, rows, cols): (usize, usize, usize)) -> RasterDataset {
    RasterDataset::new(Raster { bands, rows, cols, data: RasterData::F32(Vec::new()), ..raster(0, 0) }, true)
}

/// Three bands of `ROWS` x `COLS` float pixels, in a buffer of their own, and its first element.
fn three_bands() -> (Array3<f32>, *const f32) {
    let values: Vec<f32> = (0..3 * BAND).map(|i| i as f32).collect();
    let base = values.as_ptr();
    (Array3::from_shape_vec((3, ROWS, COLS), values).unwrap(), base)
}

/// Checks that `from_array3` refuses an array that is a part of [`three_bands`]'s buffer at `base`,
/// and gives it back exactly: the same first element, shape and strides, on the same buffer.
fn given_back(array: Array3<f32>, base: *const f32) {
    let (first, shape, strides) = (array.as_ptr(), array.raw_dim(), array.strides().to_vec());
    let (error, (_, back)) = RasterDataset::from_array3(header(array.dim()), array).unwrap_err().into_parts();
    assert!(error.to_string().contains("of the 6000 elements of its buffer"), "{error}");
    assert_eq!((back.as_ptr(), back.raw_dim(), back.strides()), (first, shape, &strides[..]));
    let (values, _) = back.into_raw_vec_and_offset();
    assert_eq!((values.as_ptr(), values.len()), (base, 3 * BAND));
}

#[test]
fn refused_arrays_come_back_exactly() {
    // Axes of length 1, with the strides ndarray gives them: 0 when sliced, 1 when inserted, the
    // array's length when collapsed, another axis's when merged, and these negated.
    let (pixels, base) = three_bands();
    let band = pixels.slice_move(s![1..2, .., ..]);
    assert_eq!(band.strides(), &[0, COLS as isize, 1]);
    given_back(band, base);
    let (pixels, base) = three_bands();
    let mut inserted = pixels.index_axis_move(Axis(0), 1).insert_axis(Axis(0));
    assert_eq!(inserted.strides(), &[1, COLS as isize, 1]);
    inserted.invert_axis(Axis(0));
    given_back(inserted, base);
    let (mut pixels, base) = three_bands();
    pixels.collapse_axis(Axis(0), 2);
    let mut moved = pixels.permuted_axes([1, 0, 2]);
    moved.invert_axis(Axis(1));
    assert_eq!(moved.strides(), &[COLS as isize, -(BAND as isize), 1]);
    given_back(moved, base);
    let (pixels, base) = three_bands();
    let mut merged = pixels.index_axis_move(Axis(0), 1).insert_axis(Axis(1));
    assert!(merged.merge_axes(Axis(0), Axis(1)) && merged.merge_axes(Axis(1), Axis(0)));
    assert_eq!((merged.dim(), merged.strides()), ((ROWS, 1, COLS), &[COLS as isize, COLS as isize, 1][..]));
    given_back(merged, base);

    // Empty arrays, whose strides are free, wherever their first element is in their buffer.
    let (pixels, base) = three_bands();
    let none = pixels.slice_move(s![1.., .., ..]).slice_move(s![..0, .., ..]);
    assert_eq!((none.as_ptr(), none.strides()), (base.wrapping_add(BAND), &[0, COLS as isize, 1][..]));
    given_back(none, base);
    let (pixels, base) = three_bands();
    let none = pixels.slice_move(s![1..2, 3..3, ..]);
    assert_eq!((none.as_ptr(), none.dim(), none.strides()), (base.wrapping_add(BAND), (1, 0, COLS), &[0, 0, 1][..]));
    given_back(none, base);
    let (pixels, base) = three_bands();
    let reversed = pixels.slice_move(s![..;-1, ..0, ..]);
    assert_eq!((reversed.as_ptr(), reversed.strides()), (base.wrapping_add(2 * BAND), &[-(BAND as isize), 0, 1][..]));
    given_back(reversed, base);

    // Other strides along axes of length 1 are refused before the array is taken apart.
    let values: Vec<f32> = (0..BAND).map(|i| i as f32).collect();
    let custom = Array3::from_shape_vec((1, ROWS, COLS).strides((7, COLS, 1)), values).unwrap();
    let first = custom.as_ptr();
    let (error, (_, back)) = RasterDataset::from_array3(header(custom.dim()), custom).unwrap_err().into_parts();
    assert!(error.to_string().starts_with("the array's axis 0, of length 1, has stride 7"), "{error}");
    assert_eq!((back.as_ptr(), back.strides()), (first, &[7, COLS as isize, 1][..]));
}

#[test]
fn empty_arrays_must_fit_the_rebuild_shape() {
    let limit = isize::MAX as usize / 2;
    for shape in [(0, limit + 1, 1), (limit + 1, 0, 1), (limit + 1, 1, 0), (0, isize::MAX as usize, 1), (0, isize::MAX as usize, 0)] {
        let array = Array3::from_shape_vec(shape.strides((0, 0, 0)), vec![1f32]).unwrap();
        let first = array.as_ptr();
        let (error, (_, back)) = RasterDataset::from_array3(header(shape), array).unwrap_err().into_parts();
        assert!(
            matches!(&error, Error::InvalidArgument(m) if m == "the empty array is too large to rebuild with an extra axis of length 2"),
            "{error}"
        );
        assert_eq!((back.as_ptr(), back.dim(), back.strides()), (first, shape, &[0, 0, 0][..]));
        assert_eq!(back.into_raw_vec_and_offset().0, vec![1f32]);
    }

    // The same check applies to two-dimensional arrays.
    let shape = (0, isize::MAX as usize);
    let array = Array2::from_shape_vec(shape.strides((0, 0)), vec![1f32]).unwrap();
    let first = array.as_ptr();
    let (error, (_, back)) = RasterDataset::from_array2(header((1, shape.0, shape.1)), array).unwrap_err().into_parts();
    assert!(matches!(error, Error::InvalidArgument(_)));
    assert_eq!((back.as_ptr(), back.dim(), back.strides()), (first, shape, &[0, 0][..]));
    assert_eq!(back.into_raw_vec_and_offset().0, vec![1f32]);

    // At the limit, the extra axis fits: a refused array is rebuilt on the same buffer.
    let shape = (0, limit, 1);
    let array = Array3::from_shape_vec(shape.strides((0, 0, 0)), vec![1f32]).unwrap();
    let first = array.as_ptr();
    let (error, (_, back)) = RasterDataset::from_array3(header(shape), array).unwrap_err().into_parts();
    assert!(error.to_string().starts_with("the array is 0 of the 1 elements of its buffer"), "{error}");
    assert_eq!((back.as_ptr(), back.dim(), back.strides()), (first, shape, &[0, 0, 0][..]));
    assert_eq!(back.into_raw_vec_and_offset().0, vec![1f32]);
}

#[test]
fn arrays_with_axes_of_length_one_move() {
    // Axes of length 1 inserted or sliced, in arrays that own their buffer: moved, nothing allocated.
    let values: Vec<f32> = (0..BAND).map(|i| i as f32).collect();
    let ptr = values.as_ptr();
    let inserted = Array2::from_shape_vec((ROWS, COLS), values).unwrap().insert_axis(Axis(0));
    let one = header((1, ROWS, COLS));
    let (ds, counts) = counted(|| RasterDataset::from_array3(one, inserted).unwrap());
    assert_eq!((counts, f32s(&ds.raster.data).as_ptr()), (NONE, ptr));
    let (pixels, one) = ds.into_array3::<f32>().unwrap();
    let sliced = pixels.slice_move(s![.., .., ..]);
    assert_eq!(sliced.strides(), &[0, COLS as isize, 1]);
    let (ds, counts) = counted(|| RasterDataset::from_array3(one, sliced).unwrap());
    assert_eq!((counts, f32s(&ds.raster.data).as_ptr()), (NONE, ptr));
}

#[test]
fn rasters_must_hold_their_data_type() {
    let mut raster = raster(3, 0);
    raster.data_type = DataType::F64;
    let error = raster.view3::<f32>().unwrap_err();
    assert!(matches!(&error, Error::InvalidData(m) if m == "the raster's data type is Float64, but its pixels are Float32"), "{error}");
    assert!(matches!(raster.view3::<f64>(), Err(Error::InvalidData(_))));
    assert!(raster.view3_mut::<f32>().is_err() && raster.band_view2::<f32>(0).is_err());
    assert!(raster.band_view2_mut::<f32>(0).is_err() && raster.split_bands_mut::<f32>().is_err());
    // A raster that could not move back is not moved into an array.
    let ptr = f32s(&raster.data).as_ptr();
    let error = RasterDataset::new(raster, true).into_array3::<f32>().unwrap_err();
    assert!(matches!(error.error(), Error::InvalidData(_)));
    let ds = error.into_input();
    assert_eq!((f32s(&ds.raster.data).as_ptr(), ds.raster.data_type), (ptr, DataType::F64));
}

#[test]
fn complex_bands_are_their_parts() {
    // Two complex bands of 2 x 2 pixels: each band's real parts, then its imaginary parts.
    let values = vec![1, 2, 3, 4, -1, -2, -3, -4, 10, 20, 30, 40, -10, -20, -30, -40];
    let raster = Raster { cols: 2, rows: 2, bands: 4, data_type: DataType::I16, data: RasterData::I16(values), ..raster(0, 0) };
    let mut ds = RasterDataset::new(raster, false);
    ds.complex = true;
    assert_eq!((ds.band_count(), ds.data_type_name()), (2, "CInt16".to_string()));
    assert_eq!(ds.raster.view3::<i16>().unwrap().dim(), (4, 2, 2));
    assert_eq!(ds.raster.band_view2::<i16>(2).unwrap(), arr2(&[[10, 20], [30, 40]]));
    assert_eq!(ds.raster.band_view2::<i16>(3).unwrap(), arr2(&[[-10, -20], [-30, -40]]));
    let (array, header) = ds.into_array3::<i16>().unwrap();
    assert_eq!(array.index_axis(Axis(0), 1), arr2(&[[-1, -2], [-3, -4]]));
    assert!(header.complex);
    assert!(RasterDataset::from_array3(header, array).unwrap().complex);
}

/// A GeoTIFF of two bands of 8 x 6 pixels (1 to 96), read lazily, and its mask (none, or one
/// pixel in five invalid).
fn tiff(masked: bool) -> (LazyRaster, Vec<u8>) {
    let values = (1..=96).collect();
    let raster = Raster {
        cols: 8,
        rows: 6,
        bands: 2,
        x_min: 100.0,
        y_min: 200.0,
        cell_size_x: 10.0,
        cell_size_y: 10.0,
        nodata: 0.0,
        data_type: DataType::U16,
        crs: CrsInfo::from_epsg(32613),
        metadata: Vec::new(),
        data: RasterData::U16(values),
    };
    let mask: Vec<u8> = (0..48).map(|i| if i % 5 == 0 { 0 } else { 255 }).collect();
    let options = RasterWriteOptions { mask: masked.then(|| mask.clone()), ..Default::default() };
    let bytes = terrio::memory::write_raster_bytes(&RasterDataset::new(raster, true), "masked.tif", &options).unwrap();
    (LazyRaster::from_tiff_bytes(bytes).unwrap().unwrap(), mask)
}

#[test]
fn lazy_windows_move_into_arrays() {
    let (mut lazy, _) = tiff(true);
    let window = Window::new(2, 1, 4, 3);
    let read = lazy.read_window(window, &WindowOptions::default()).unwrap();
    let (ptr, gt) = (read.raster.data.as_u16_slice().unwrap().as_ptr(), read.geo_transform());
    let ((array, header), counts) = counted(|| read.into_array3::<u16>().unwrap());
    assert_eq!(counts, NONE);
    assert_eq!((array.as_ptr(), array.dim()), (ptr, (2, 3, 4)));
    let expected = Array3::from_shape_fn((2, 3, 4), |(b, r, c)| (b * 48 + (r + 1) * 8 + c + 2 + 1) as u16);
    assert_eq!(array, expected);
    assert_eq!(gt, [120.0, 10.0, 0.0, 250.0, 0.0, -10.0]);
    assert_eq!(header.geo_transform(), gt);
    assert_eq!(header.band_nodata, vec![Some(0.0), Some(0.0)]);
    let band = lazy.read_window(window, &WindowOptions { bands: vec![2], overview: None }).unwrap();
    assert_eq!(band.into_array2::<u16>().unwrap().0, array.index_axis(Axis(0), 1));
}

#[test]
fn lazy_masks_are_arrays() {
    let (mut lazy, mask) = tiff(true);
    let window = Window::new(2, 1, 4, 3);
    let read = lazy.read_mask_array2(window, None).unwrap().unwrap();
    assert_eq!(read, Array2::from_shape_fn((3, 4), |(r, c)| mask[(r + 1) * 8 + c + 2]));
    assert!(read.iter().any(|v| *v == 0) && read.iter().any(|v| *v == 255));
    assert_eq!(read.as_slice(), lazy.read_mask(window, None).unwrap().as_deref());
    // The array is the buffer `read_mask` reads: it allocates nothing more.
    let (_, reading) = counted(|| lazy.read_mask(window, None).unwrap());
    let (_, adapting) = counted(|| lazy.read_mask_array2(window, None).unwrap());
    assert_eq!(adapting, reading);
    // The pixels are not masked; a raster without a mask has none.
    let pixels = lazy.read_window(window, &WindowOptions::default()).unwrap();
    assert_eq!(pixels.raster.data.as_u16_slice().unwrap()[0], 1 + 8 + 2);
    assert_eq!(tiff(false).0.read_mask_array2(window, None).unwrap(), None);
}
