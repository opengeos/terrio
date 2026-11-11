"""Complex GeoTIFF writes and copies against GDAL 3.13.3, including exact Int32 components."""

import numpy as np
import pytest

import terrio
import terrio.rio as rio
from terrio.rio.crs import CRS
from terrio.rio.io import DatasetWriter
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


@pytest.mark.parametrize("mode", ["whole", "window", "nearest", "out", "boundless"])
def test_read_only_cint32_wide_output_keeps_integer_components(tmp_path, gdal, mode):
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
    options = {"out_dtype": "complex128"}
    if mode == "window":
        options["window"] = Window(3, 2, 5, 4)
        expected = values[:, 2:6, 3:8]
    elif mode == "nearest":
        options["out_shape"] = (2, 5, 7)
        expected = oracle.ReadAsArray(buf_xsize=7, buf_ysize=5, buf_type=gdal.GDT_CFloat64)
    elif mode == "out":
        options = {"out": np.empty_like(values)}
    elif mode == "boundless":
        options.update(window=Window(-2, -1, 41, 21), boundless=True)
        expected = np.pad(values, ((0, 0), (1, 1), (2, 2)))
    with rio.open(path) as source:
        assert source.read().dtype == np.dtype("complex64")
        actual = source.read(**options)
    assert actual.dtype == np.dtype("complex128")
    np.testing.assert_array_equal(actual, expected)
