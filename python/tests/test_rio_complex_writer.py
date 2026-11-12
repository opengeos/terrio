"""Complex GeoTIFF writes and copies against GDAL 3.13.3, including exact Int32 components."""

import sys

import numpy as np
import pytest

import terrio
import terrio.rio as rio
from terrio.rio.crs import CRS
from terrio.rio.io import DatasetReader, DatasetWriter
from terrio.rio.shutil import copy
from terrio.rio.windows import Window

GT = (500000.0, 10.0, 0.0, 5800000.0, 0.0, -10.0)


def read_precise(gdal, path):
    ds = gdal.Open(str(path))
    return ds.ReadAsArray(buf_type=gdal.GDT_CFloat64), ds


@pytest.mark.parametrize("driver", ["GTiff", "COG"])
@pytest.mark.parametrize("dtype", ["complex64", "complex128", "complex_int16"])
@pytest.mark.parametrize("limit", ["1", "100000000"])
def test_complex_window_writes(tmp_path, monkeypatch, gdal, driver, dtype, limit):
    monkeypatch.setenv("TERRIO_RIO_IN_MEMORY_LIMIT", limit)
    data = (np.arange(2 * 37 * 23).reshape(2, 37, 23) - 500) * (0.25 - 0.5j)
    if dtype == "complex_int16":
        data = data.real.round() + 1j * data.imag.round()
    data = np.asfortranarray(data.astype("complex128" if dtype == "complex128" else "complex64"))
    path = tmp_path / "written.tif"
    with rio.open(
        path,
        "w",
        driver=driver,
        dtype=dtype,
        width=23,
        height=37,
        count=2,
        crs="EPSG:32633",
        transform=rio.Affine.from_gdal(*GT),
        nodata=-9999,
        tiled=True,
        blockxsize=16,
        blockysize=16,
        compress="deflate",
    ) as dst:
        for start in range(0, 37, 7):
            rows = min(7, 37 - start)
            dst.write(data[:, start : start + rows], window=Window(0, start, 23, rows))
        dst.update_tags(example="complex")
        dst.set_band_description(1, "real and imaginary")
    actual, ds = read_precise(gdal, path)
    np.testing.assert_array_equal(actual, data)
    types = {
        "complex64": gdal.GDT_CFloat32,
        "complex128": gdal.GDT_CFloat64,
        "complex_int16": gdal.GDT_CInt16,
    }
    assert ds.RasterCount == 2
    assert ds.GetRasterBand(1).DataType == types[dtype]
    assert ds.GetGeoTransform() == GT
    assert ds.GetRasterBand(1).GetNoDataValue() == -9999
    assert ds.GetMetadataItem("example") == "complex"
    assert ds.GetRasterBand(1).GetDescription() == "real and imaginary"
    with rio.open(path) as src:
        assert src.count == 2
        assert src.dtypes == (dtype,) * 2
        np.testing.assert_array_equal(src.read(), data)


@pytest.mark.parametrize("driver", ["GTiff", "COG"])
@pytest.mark.parametrize("kind", ["CInt16", "CInt32", "CFloat32", "CFloat64"])
@pytest.mark.parametrize("limit", ["1", "100000000"])
def test_complex_copies_preserve_physical_type_and_values(
    tmp_path, monkeypatch, gdal, driver, kind, limit
):
    monkeypatch.setenv("TERRIO_RIO_IN_MEMORY_LIMIT", limit)
    source = tmp_path / "source.tif"
    data = np.arange(2 * 37 * 23).reshape(2, 37, 23).astype("float64") - 200
    if kind == "CInt32":
        data += 2**24 + 1
    data = data + 1j * (-data - 3)
    ds = gdal.GetDriverByName("GTiff").Create(str(source), 23, 37, 2, getattr(gdal, "GDT_" + kind))
    ds.SetGeoTransform(GT)
    ds.SetProjection(CRS.from_epsg(32633).to_wkt())
    for band in (1, 2):
        ds.GetRasterBand(band).WriteArray(data[band - 1])
    ds = None
    target = tmp_path / "copy.tif"
    copy(source, target, driver=driver, compress="deflate")
    actual, out = read_precise(gdal, target)
    np.testing.assert_array_equal(actual, data)
    assert out.RasterCount == 2
    assert out.GetRasterBand(1).DataType == getattr(gdal, "GDT_" + kind)
    assert out.GetGeoTransform() == GT
    assert out.GetSpatialRef().GetAuthorityCode(None) == "32633"


def test_complex_array_precision_and_component_conversion(tmp_path, gdal):
    values = np.array([[2**24 + 1 - (2**24 + 3) * 1j]], dtype="complex128")
    raster = terrio.Raster.from_array(values, GT, complex_dtype="CInt32")
    assert raster.data_type == "CInt32"
    np.testing.assert_array_equal(raster.read(1, out_dtype="complex128"), values)
    np.testing.assert_array_equal(np.asarray(raster, dtype="complex128")[0], values)
    path = tmp_path / "precise.tif"
    raster.write(str(path))
    actual, ds = read_precise(gdal, path)
    np.testing.assert_array_equal(actual, values)
    assert ds.GetRasterBand(1).DataType == gdal.GDT_CInt32


@pytest.mark.parametrize("driver", ["GTiff", "COG"])
def test_complex_masked_writes_keep_the_dataset_mask(tmp_path, gdal, driver):
    path = tmp_path / "masked.tif"
    values = np.arange(32 * 32, dtype="float32").reshape(1, 32, 32).astype("complex64") * (1 - 2j)
    mask = np.full((32, 32), 255, dtype="uint8")
    mask[3:8, 5:13] = 0
    with rio.open(path, "w", driver=driver, width=32, height=32, count=1, dtype="complex64") as dst:
        dst.write(values)
        dst.write_mask(mask)
    actual, ds = read_precise(gdal, path)
    np.testing.assert_array_equal(actual, values[0])
    np.testing.assert_array_equal(ds.GetRasterBand(1).GetMaskBand().ReadAsArray(), mask)


def test_integer_complex_conversion_matches_gdal(tmp_path, gdal):
    parts = np.array([-40000.25, -32768.6, -1.5, -0.5, 0.5, 32767.6, 40000.25])
    values = (parts + 1j * parts[::-1]).reshape(1, -1)
    reference = tmp_path / "reference.tif"
    ds = gdal.GetDriverByName("GTiff").Create(str(reference), len(parts), 1, 1, gdal.GDT_CInt16)
    ds.GetRasterBand(1).WriteArray(values)
    ds = None
    expected, _ = read_precise(gdal, reference)
    actual = terrio.Raster.from_array(values, GT, complex_dtype="CInt16").read(1)
    np.testing.assert_array_equal(actual, expected)


@pytest.mark.parametrize("driver", ["GTiff", "COG"])
@pytest.mark.parametrize("limit", ["1", "2000", "100000000"])
@pytest.mark.parametrize("mode", ["w+", "r+"])
def test_integer_complex_window_rounding_matches_gdal(
    tmp_path, monkeypatch, gdal, driver, limit, mode
):
    """CInt16 writes, updates and readable-writer copies round once, as GDAL 3.13.3 does."""
    monkeypatch.setenv("TERRIO_RIO_IN_MEMORY_LIMIT", limit)
    parts = np.array([2.4999999, -2.4999999, 0.49999999, -0.49999999, 32766.4999, -32767.4999])
    values = np.tile(parts + 1j * parts[::-1], (37, 1))
    reference = tmp_path / "rounding-reference.tif"
    ds = gdal.GetDriverByName("GTiff").Create(str(reference), len(parts), 37, 1, gdal.GDT_CInt16)
    ds.GetRasterBand(1).WriteArray(values)
    expected = ds.ReadAsArray(buf_type=gdal.GDT_CFloat64)
    path = tmp_path / "rounding.tif"
    options = dict(
        driver=driver,
        width=len(parts),
        height=37,
        count=1,
        dtype="complex_int16",
        tiled=True,
        blockxsize=16,
        blockysize=16,
    )
    if mode == "r+":
        seed = gdal.GetDriverByName("MEM").Create("", len(parts), 37, 1, gdal.GDT_CInt16)
        original = gdal.GetDriverByName(driver).CreateCopy(str(path), seed)
        original.Close()
        seed.Close()
        options = {}
    with rio.open(path, mode, **options) as dst:
        assert dst._data.dtype == np.dtype("complex128")
        assert isinstance(dst._data, np.memmap) == (values.nbytes > int(limit))
        for start in range(0, 37, 7):
            rows = min(7, 37 - start)
            dst.write(values[None, start : start + rows], window=Window(0, start, len(parts), rows))
        copied = tmp_path / "rounding-copy.tif"
        copy(dst, copied, driver=driver)
        copied_values, copied_output = read_precise(gdal, copied)
        assert copied_output.GetRasterBand(1).DataType == gdal.GDT_CInt16
        np.testing.assert_array_equal(copied_values, expected)
    actual, output = read_precise(gdal, path)
    assert output.GetRasterBand(1).DataType == gdal.GDT_CInt16
    np.testing.assert_array_equal(actual, expected)


def test_real_landsat_complex_integer_copy_is_exact(tmp_path, data_dir, gdal):
    with rio.open(data_dir / "landsat7.tif") as src:
        sample = src.read([1, 2], window=Window(500, 500, 64, 64))
        crs = src.crs.to_wkt()
        transform = src.window_transform(Window(500, 500, 64, 64)).to_gdal()
    values = (2**24 + sample[0].astype("float64") + 1) + 1j * (
        -(2**24) - sample[1].astype("float64") - 3
    )
    source = tmp_path / "landsat-complex.tif"
    terrio.Raster.from_array(values, transform, crs=crs, complex_dtype="CInt32").write(str(source))
    for driver in ("GTiff", "COG"):
        target = tmp_path / f"landsat-{driver}.tif"
        copy(source, target, driver=driver, compress="deflate")
        actual, ds = read_precise(gdal, target)
        np.testing.assert_array_equal(actual, values)
        assert ds.GetGeoTransform() == transform


def test_integer_complex_copy_from_an_overview_keeps_precision(tmp_path, gdal):
    source = tmp_path / "overviews.tif"
    values = (np.arange(64 * 64).reshape(64, 64).astype("float64") + 2**24 + 1) * (1 - 1j)
    ds = gdal.GetDriverByName("GTiff").Create(str(source), 64, 64, 1, gdal.GDT_CInt32)
    ds.GetRasterBand(1).WriteArray(values)
    ds.BuildOverviews("NEAREST", [2])
    expected = ds.GetRasterBand(1).GetOverview(0).ReadAsArray(buf_type=gdal.GDT_CFloat64)
    ds = None
    target = tmp_path / "overview-copy.tif"
    with rio.open(source, overview_level=0) as src:
        copy(src, target, driver="COG")
    actual, ds = read_precise(gdal, target)
    np.testing.assert_array_equal(actual, expected)
    assert ds.RasterCount == 1 and ds.GetRasterBand(1).DataType == gdal.GDT_CInt32


@pytest.mark.parametrize("dtype", ["complex_int32", "complex_int8"])
def test_unsupported_integer_complex_dtype_has_a_specific_error(tmp_path, dtype):
    with pytest.raises(TypeError, match="unsupported complex integer dtype"):
        DatasetWriter(
            tmp_path / "invalid.tif", "w", driver="GTiff", width=1, height=1, count=1, dtype=dtype
        )
    assert not (tmp_path / "invalid.tif").exists()


@pytest.mark.parametrize("driver", ["GTiff", "COG"])
@pytest.mark.parametrize("limit", ["1", "100000000"])
def test_integer_complex_update_and_copy_keep_exact_components(
    tmp_path, monkeypatch, gdal, driver, limit
):
    """A readable writer retains CInt32 storage while ordinary reads stay complex64."""
    monkeypatch.setenv("TERRIO_RIO_IN_MEMORY_LIMIT", limit)
    values = (np.arange(2 * 19 * 37).reshape(2, 19, 37).astype("float64") + 2**24 + 1) * (1 - 1j)
    values[0, 0, 0] = (2**31 - 1) - (2**31) * 1j
    source = tmp_path / "update.tif"
    ds = gdal.GetDriverByName("GTiff").Create(str(source), 37, 19, 2, gdal.GDT_CInt32)
    ds.SetGeoTransform(GT)
    ds.SetProjection(CRS.from_epsg(32633).to_wkt())
    for band in range(2):
        ds.GetRasterBand(band + 1).WriteArray(values[band])
    ds = None
    target = tmp_path / "copy.tif"
    with rio.open(source, "r+") as src:
        assert src.read().dtype == np.dtype("complex64")
        np.testing.assert_array_equal(src.read(out_dtype="complex128"), values)
        replacement = np.array([[16777219 - 16777223j]], dtype="complex128")
        src.write(replacement, 2, window=Window(3, 2, 1, 1))
        values[1, 2, 3] = replacement[0, 0]
        copy(src, target, driver=driver, compress="deflate")
    for path in (source, target):
        actual, ds = read_precise(gdal, path)
        assert ds.RasterCount == 2
        assert ds.GetRasterBand(1).DataType == gdal.GDT_CInt32
        assert ds.GetGeoTransform() == GT
        np.testing.assert_array_equal(actual, values)


@pytest.mark.parametrize("kind", ["CInt16", "CInt32", "CFloat32", "CFloat64"])
def test_complex_update_keeps_the_declared_sample_type(tmp_path, gdal, kind):
    values = (np.arange(15).reshape(3, 5) - 7) * (1 - 1j)
    if kind.startswith("CFloat"):
        values = values * 0.25
    source = tmp_path / "complex-update.tif"
    ds = gdal.GetDriverByName("GTiff").Create(str(source), 5, 3, 1, getattr(gdal, "GDT_" + kind))
    ds.GetRasterBand(1).WriteArray(values)
    ds = None
    with rio.open(source, "r+") as dst:
        dst.update_tags(review="preserved sample type")
    actual, ds = read_precise(gdal, source)
    assert ds.GetRasterBand(1).DataType == getattr(gdal, "GDT_" + kind)
    np.testing.assert_array_equal(actual, values)


@pytest.mark.parametrize("dtype", ["complex128", ">c16"])
@pytest.mark.parametrize("mode", ["whole", "window", "nearest", "out", "boundless"])
def test_read_only_cint32_wide_output_keeps_integer_components(tmp_path, gdal, mode, dtype):
    """GDAL 3.13.3 CInt32 components survive a requested complex128 output buffer."""
    values = (np.arange(2 * 19 * 37).reshape(2, 19, 37).astype("float64") + 2**24 + 1) * (1 - 1j)
    values[0, 0, 0] = (2**31 - 1) - (2**31) * 1j
    path = tmp_path / "read-only.tif"
    ds = gdal.GetDriverByName("GTiff").Create(str(path), 37, 19, 2, gdal.GDT_CInt32)
    ds.SetGeoTransform(GT)
    for band in range(2):
        ds.GetRasterBand(band + 1).WriteArray(values[band])
    ds = None
    expected, oracle = read_precise(gdal, path)
    np.testing.assert_array_equal(expected, values)
    options = {"out_dtype": dtype}
    if mode == "window":
        options["window"] = Window(3, 2, 5, 4)
        expected = values[:, 2:6, 3:8]
    elif mode == "nearest":
        options["out_shape"] = (2, 5, 7)
        expected = oracle.ReadAsArray(buf_xsize=7, buf_ysize=5, buf_type=gdal.GDT_CFloat64)
    elif mode == "out":
        options = {"out": np.empty_like(values, dtype=dtype)}
    elif mode == "boundless":
        options.update(window=Window(-2, -1, 41, 21), boundless=True)
        expected = np.pad(values, ((0, 0), (1, 1), (2, 2)))
    with rio.open(path) as source:
        assert source.read().dtype == np.dtype("complex64")
        actual = source.read(**options)
    assert actual.dtype == np.dtype(dtype)
    np.testing.assert_array_equal(actual, expected)


@pytest.mark.parametrize("dtype", ["float64", "int32", "uint32", "int64", "uint64"])
def test_read_only_cint32_real_output_keeps_integer_components(tmp_path, gdal, dtype):
    """Read Int32 real parts without a float32 intermediate, as GDAL does.

    Args:
        tmp_path: Temporary directory fixture.
        gdal: GDAL reference module.
        dtype: Real NumPy output type that can hold the positive test values.
    """
    values = (np.arange(2 * 19 * 37).reshape(2, 19, 37).astype("float64") + 2**24 + 1) * (1 - 1j)
    values[0, 0, 0] = (2**31 - 1) - (2**31) * 1j
    path = tmp_path / "read-only-real.tif"
    ds = gdal.GetDriverByName("GTiff").Create(str(path), 37, 19, 2, gdal.GDT_CInt32)
    ds.SetGeoTransform(GT)
    for band in range(2):
        ds.GetRasterBand(band + 1).WriteArray(values[band])
    ds = None
    with rio.open(path) as source:
        actual = source.read(out_dtype=dtype)
    assert actual.dtype == np.dtype(dtype)
    np.testing.assert_array_equal(actual, values.real.astype(dtype))
    for source in (
        terrio.open_raster(str(path)),
        terrio.Raster.from_array(values, GT, complex_dtype="CInt32"),
    ):
        actual = source.read(out_dtype=dtype)
        assert actual.dtype == np.dtype(dtype)
        np.testing.assert_array_equal(actual, values.real.astype(dtype))


@pytest.mark.parametrize("byteorder", ["<", ">"])
@pytest.mark.parametrize("complex_input", [False, True])
def test_numpy_dtype_casts_keep_requested_byte_order(byteorder, complex_input):
    """Explicit read and NumPy conversion retain byte order and Int32 component precision."""
    values = np.arange(6, dtype="int16").reshape(2, 3)
    if complex_input:
        values = (values.astype("float64") + 2**24 + 1) * (1 - 1j)
        source = terrio.Raster.from_array(values, GT, complex_dtype="CInt32")
        dtype = np.dtype(byteorder + "c16")
    else:
        source = terrio.Raster.from_array(values, GT)
        dtype = np.dtype(byteorder + "i2")
    for actual in (
        source.read(1, out_dtype=dtype.str),
        source.read(1, out_dtype=dtype),
        np.asarray(source, dtype=dtype)[0],
    ):
        assert actual.dtype == dtype
        np.testing.assert_array_equal(actual, values)


@pytest.mark.parametrize("driver", ["GTiff", "COG"])
def test_reader_subclass_copy_keeps_exact_integer_components(tmp_path, gdal, driver):
    """An inherited native reader preserves CInt32 components as its base reader does."""

    class Reader(DatasetReader):
        pass

    values = np.array([[2**24 + 1 - (2**24 + 3) * 1j]], dtype="complex128")
    path = tmp_path / "subclass-source.tif"
    source = gdal.GetDriverByName("GTiff").Create(str(path), 1, 1, 1, gdal.GDT_CInt32)
    source.GetRasterBand(1).WriteArray(values)
    source = None
    target = tmp_path / "subclass-copy.tif"
    with Reader(path) as reader:
        np.testing.assert_array_equal(reader.read(1, out_dtype="complex128"), values)
        copy(reader, target, driver=driver)
    actual, dataset = read_precise(gdal, target)
    assert dataset.GetRasterBand(1).DataType == gdal.GDT_CInt32
    np.testing.assert_array_equal(actual, values)


@pytest.mark.parametrize("layout", ["c", "fortran", "strided", "transposed"])
@pytest.mark.parametrize("dtype", ["complex64", "complex128"])
@pytest.mark.parametrize("kind", ["CInt16", "CInt32"])
def test_integer_complex_array_layouts_match_gdal(tmp_path, gdal, layout, dtype, kind):
    """Direct component conversion preserves layout, rounding and saturation against GDAL 3.13.3."""
    parts = np.array([-40000.25, -32768.6, -1.5, -0.5, 0.5, 32767.6, 40000.25, 2**24 + 1])
    values = ((parts + 1j * parts[::-1]).reshape(2, 4) * np.ones((2, 1, 1))).astype(dtype)
    if layout == "fortran":
        values = np.asfortranarray(values)
    elif layout == "strided":
        values = values[:, :, ::2]
    elif layout == "transposed":
        values = values.transpose(0, 2, 1)
    count, rows, cols = values.shape
    path = tmp_path / "layout-reference.tif"
    reference = gdal.GetDriverByName("GTiff").Create(
        str(path), cols, rows, count, getattr(gdal, "GDT_" + kind)
    )
    for index in range(count):
        reference.GetRasterBand(index + 1).WriteArray(values[index])
    expected = reference.ReadAsArray(buf_type=gdal.GDT_CFloat64)
    actual = terrio.Raster.from_array(values, GT, complex_dtype=kind).read(out_dtype="complex128")
    np.testing.assert_array_equal(actual, expected)


def test_boundless_fallback_preserves_requested_numpy_complex_dtype(tmp_path, gdal):
    """The fallback forwards explicit dtype objects before losing Int32 precision."""
    from terrio.rio.enums import Resampling
    from terrio.rio.windows import Window

    values = np.full((2, 3), (2**24 + 1) * (1 - 1j), dtype="complex128")
    path = tmp_path / "fallback-complex.tif"
    dataset = gdal.GetDriverByName("GTiff").Create(str(path), 3, 2, 1, gdal.GDT_CInt32)
    dataset.SetGeoTransform(GT)
    dataset.GetRasterBand(1).WriteArray(values)
    dataset = None
    with rio.open(path) as reader:
        actual, inside = reader._boundless(
            [1],
            Window(-1, -1, 5, 4),
            (4, 5),
            Resampling.nearest,
            0,
            out_dtype=np.dtype("complex128"),
        )
    assert actual.dtype == np.dtype("complex128")
    np.testing.assert_array_equal(actual[0], np.pad(values, ((1, 1), (1, 1))))
    np.testing.assert_array_equal(inside[0], np.pad(np.ones((2, 3), dtype=bool), ((1, 1), (1, 1))))


@pytest.mark.parametrize("limit", ["1", "100000000"])
@pytest.mark.parametrize("mode", ["r+", "w+"])
@pytest.mark.parametrize(
    "dtype", ["int32", "uint32", "int64", "uint64", "float64", "complex128", "complex64"]
)
def test_readable_cint32_writer_boundless_output_preserves_components(
    tmp_path, monkeypatch, gdal, limit, mode, dtype
):
    """Readable memory/spill writers preserve wide requested components outside their grid."""
    monkeypatch.setenv("TERRIO_RIO_IN_MEMORY_LIMIT", limit)
    values = np.full((1, 2, 3), (2**24 + 1) * (1 - 1j), dtype="complex128")
    values[0, 0, 0] = (2**31 - 1) - (2**31) * 1j
    path = tmp_path / "boundless-update.tif"
    if mode == "r+":
        dataset = gdal.GetDriverByName("GTiff").Create(str(path), 3, 2, 1, gdal.GDT_CInt32)
        dataset.SetGeoTransform(GT)
        dataset.GetRasterBand(1).WriteArray(values[0])
        dataset = None
        options = {}
    else:
        options = dict(
            driver="GTiff",
            dtype="complex128",
            _complex_dtype="CInt32",
            width=3,
            height=2,
            count=1,
            transform=rio.Affine.from_gdal(*GT),
        )
    with rio.open(path, mode, **options) as reader:
        if mode == "w+":
            reader.write(values)
        actual = reader.read(window=Window(-1, -1, 5, 4), boundless=True, out_dtype=dtype)
    expected = np.pad(values, ((0, 0), (1, 1), (1, 1)))
    if np.dtype(dtype).kind != "c":
        expected = expected.real
    assert actual.dtype == np.dtype(dtype)
    np.testing.assert_array_equal(actual, expected.astype(dtype))


@pytest.mark.parametrize(
    "dtype,sample",
    [
        ("complex128", "CInt16"),
        ("complex64", "CInt32"),
        ("complex128", "CFloat32"),
        ("complex64", "CFloat64"),
        ("complex_int16", "CFloat32"),
        ("complex_int16", "CFloat64"),
        ("complex_int16", "CInt32"),
    ],
)
def test_complex_writer_rejects_a_conflicting_physical_sample_selector(tmp_path, dtype, sample):
    """Private copy metadata cannot select storage inconsistent with the writer buffer."""
    path = tmp_path / "conflicting.tif"
    with pytest.raises(TypeError, match="needs dtype|complex_int16 is CInt16"):
        rio.open(
            path,
            "w",
            driver="GTiff",
            dtype=dtype,
            _complex_dtype=sample,
            width=3,
            height=2,
            count=1,
            transform=rio.Affine.from_gdal(*GT),
        )
    assert not path.exists()


def test_integer_complex_copy_reports_native_read_errors(tmp_path, monkeypatch, gdal):
    """A failed precise CInt32 copy read uses the rio exception contract."""
    from terrio.rio.errors import RasterioIOError

    source = tmp_path / "source.tif"
    ds = gdal.GetDriverByName("GTiff").Create(str(source), 2, 2, 1, gdal.GDT_CInt32)
    ds.Close()
    with rio.open(source) as reader:
        native = reader._raster

        class FailedRead:
            def read(self, *args, **kwargs):
                """Report the injected native read failure."""
                raise ValueError("injected read failure")

        with monkeypatch.context() as patch:
            patch.setattr(reader, "_raster", FailedRead())
            with pytest.raises(RasterioIOError, match="injected read failure") as caught:
                reader._read_for_copy()
            assert isinstance(caught.value.__cause__, ValueError)
        assert reader._raster is native


@pytest.mark.parametrize("dtype", ["uint8", "float64"])
def test_real_copy_uses_cached_sample_metadata(tmp_path, monkeypatch, gdal, dtype):
    """Copying does not reborrow the native sample-type getter during another read."""
    source, target = tmp_path / "real.tif", tmp_path / "copied.tif"
    values = np.arange(24, dtype=dtype).reshape(1, 4, 6)
    with rio.open(source, "w", driver="GTiff", width=6, height=4, count=1, dtype=dtype) as dst:
        dst.write(values)
    with rio.open(source) as reader:
        native = reader._raster

        class ReadingRaster:
            @property
            def data_type(self):
                """Refuse a repeated native sample-type borrow."""
                raise AssertionError("native sample type is already borrowed by a read")

            def __getattr__(self, name):
                """Forward other operations to the actual native raster."""
                return getattr(native, name)

        with monkeypatch.context() as patch:
            patch.setattr(reader, "_raster", ReadingRaster())
            copy(reader, target, driver="GTiff")
    decoded = gdal.Open(str(target))
    np.testing.assert_array_equal(decoded.ReadAsArray(), values[0])


def test_integer_complex_copy_preserves_resource_refusal(tmp_path, monkeypatch, gdal):
    """The precise-copy path preserves resource admission errors (GDAL 3.13.3 fixture)."""
    from terrio.errors import ResourceLimitError

    source = tmp_path / "source.tif"
    ds = gdal.GetDriverByName("GTiff").Create(str(source), 2, 2, 1, gdal.GDT_CInt32)
    ds.Close()
    refusal = ResourceLimitError("injected admission refusal")
    with rio.open(source) as reader:

        class RefusedRead:
            def read(self, *args, **kwargs):
                """Preserve the injected resource refusal unchanged."""
                raise refusal

        with monkeypatch.context() as patch:
            patch.setattr(reader, "_raster", RefusedRead())
            with pytest.raises(ResourceLimitError) as caught:
                reader._read_for_copy()
            assert caught.value is refusal


@pytest.mark.skipif(sys.platform != "linux", reason="Linux usable-memory policy")
@pytest.mark.parametrize(
    "surface", ["rio-copy", "native-read", "native-array", "rio-working-set", "native-working-set"]
)
def test_precise_complex_copy_checks_wide_output_before_allocation(tmp_path, gdal, surface):
    """A 72 MiB CInt32 source cannot produce a 144 MiB buffer under a 128 MiB policy."""
    import os
    import subprocess
    import sys

    source = tmp_path / "wide-copy.tif"
    dataset = gdal.GetDriverByName("GTiff").Create(
        str(source),
        1024,
        9216,
        1,
        gdal.GDT_CInt32,
        options=["TILED=YES", "COMPRESS=DEFLATE"],
    )
    dataset.GetRasterBand(1).Fill(16777217, -16777219)
    dataset.Close()
    script = """
import resource
import sys
import terrio
import numpy as np
from terrio.rio import open
from terrio.rio.windows import Window
_, hard = resource.getrlimit(resource.RLIMIT_RSS)
resource.setrlimit(resource.RLIMIT_RSS, (min(hard, 128 << 20) if hard >= 0 else 128 << 20, hard))
surface = sys.argv[2]
if surface.startswith("rio"):

    reader = open(sys.argv[1])
    small = reader._read_for_copy(Window(0, 0, 16, 16))
    if surface == "rio-copy":
        large = reader._read_for_copy
    else:
        large = lambda: reader._read_for_copy(Window(0, 0, 1024, 4096))
else:
    reader = terrio.open_raster(sys.argv[1], lazy=True)
    small = reader.read(window=(0, 0, 16, 16), out_dtype="complex128")
    if surface == "native-working-set":
        large = lambda: reader.read(window=(0, 0, 1024, 4096), out_dtype="complex128")
    elif surface == "native-read":
        large = lambda: reader.read(out_dtype="complex128")
    else:
        large = lambda: np.asarray(reader, dtype="complex128")
assert small.dtype.name == "complex128"
assert (small == complex(16777217, -16777219)).all()
try:
    large()
except terrio.ResourceLimitError:
    pass
else:
    raise AssertionError("unchecked promoted complex output or conversion working set")
"""
    environment = dict(os.environ)
    for option in ("TERRIO_MAX_DECLARED_BYTES", "TERRIO_MAX_BANDS", "TERRIO_TIFF_MAX_CHUNK_BYTES"):
        environment.pop(option, None)
    result = subprocess.run(
        [sys.executable, "-c", script, str(source), surface],
        env=environment,
        capture_output=True,
        text=True,
        timeout=60,
    )
    assert result.returncode == 0, result.stdout + result.stderr
