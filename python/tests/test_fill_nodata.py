"""Fill-nodata bindings and GDAL comparisons."""

import numpy as np
import pytest

import terrio

TRANSFORM = (100.0, 2.0, 0.25, 200.0, 0.5, -2.0)


def raster(a, nodata=None):
    return terrio.Raster.from_array(a, TRANSFORM, crs="EPSG:32610", nodata=nodata)


def test_fill_nodata_api(tmp_path):
    a = np.array([[0, 1, 2], [3, -9999, 5], [6, 7, 8]], dtype="float32")
    src = raster(a, -9999)
    path = str(tmp_path / "filled.tif")
    out = terrio.fill_nodata(src, path, max_distance=1)
    assert out.read(1)[1, 1] == np.float32(13 / 3)
    np.testing.assert_array_equal(out.read(1)[a != -9999], a[a != -9999])
    assert out.transform == TRANSFORM
    assert out.crs == "EPSG:32610"
    assert out.nodata == -9999
    np.testing.assert_array_equal(terrio.open_raster(path).read(1), out.read(1))
    assert src.read(1)[1, 1] == -9999
    nearest = terrio.fill_nodata(src, interpolation="nearest")
    assert nearest.read(1)[1, 1] == 1
    mask = raster(np.zeros(a.shape, dtype="uint8"))
    np.testing.assert_array_equal(terrio.fill_nodata(src, mask=mask).read(1), a)
    with pytest.raises(terrio.errors.InvalidArgumentError):
        terrio.fill_nodata(src, max_distance=-1)


@pytest.mark.parametrize("explicit", [False, True])
@pytest.mark.parametrize("method", ["inv_dist", "nearest"])
@pytest.mark.parametrize("smooth", [0, 1, 3])
@pytest.mark.parametrize("distance", [0, 2, 10])
@pytest.mark.parametrize("dtype", ["uint8", "int16", "float32"])
def test_against_gdal(method, smooth, distance, dtype, explicit):
    gdal = pytest.importorskip("osgeo.gdal")
    gdal.UseExceptions()
    rng = np.random.default_rng(238)
    a = rng.integers(1, 240, size=(12, 16)).astype(dtype)
    mask = rng.integers(0, 3, size=a.shape).astype("uint8") * 100
    mask[2:5, 7:11] = 0
    nodata = -9999 if dtype == "float32" else 0
    a[mask == 0] = nodata
    gdt = {"uint8": gdal.GDT_Byte, "int16": gdal.GDT_Int16, "float32": gdal.GDT_Float32}
    ds = gdal.GetDriverByName("MEM").Create("", 16, 12, 1, gdt[dtype])
    band = ds.GetRasterBand(1)
    band.WriteArray(a)
    band.SetNoDataValue(nodata)
    m = gdal.GetDriverByName("MEM").Create("", 16, 12, 1, gdal.GDT_Byte)
    m.GetRasterBand(1).WriteArray(mask)
    gdal.FillNodata(
        band,
        m.GetRasterBand(1) if explicit else None,
        distance,
        smooth,
        [f"INTERPOLATION={method}"],
    )
    out = terrio.fill_nodata(
        raster(a, nodata),
        mask=raster(mask) if explicit else None,
        max_distance=distance,
        interpolation=method,
        smoothing_iterations=smooth,
    ).read(1)
    np.testing.assert_array_equal(out, band.ReadAsArray())
    np.testing.assert_array_equal(out[mask != 0], a[mask != 0])


def test_nan_mask_selected_band_and_exact_donors():
    a = np.array([[1.123456789, np.nan, 3.987654321]], dtype="float64")
    stack = np.stack([np.zeros_like(a), a])
    out = terrio.fill_nodata(raster(stack, np.nan), band=2, smoothing_iterations=2)
    assert out.read(1)[0, 0] == a[0, 0]
    assert out.read(1)[0, 2] == a[0, 2]
    assert np.isfinite(out.read(1)).all()


def test_internal_mask_paths_and_lazy_inputs(tmp_path):
    gdal = pytest.importorskip("osgeo.gdal")
    gdal.UseExceptions()
    path = str(tmp_path / "masked.tif")
    ds = gdal.GetDriverByName("GTiff").Create(path, 3, 1, 1, gdal.GDT_Float32)
    band = ds.GetRasterBand(1)
    band.WriteArray(np.array([[1, 99, 3]], dtype="float32"))
    band.CreateMaskBand(gdal.GMF_PER_DATASET)
    band.GetMaskBand().WriteArray(np.array([[255, 0, 255]], dtype="uint8"))
    ds = None
    for src in [path, terrio.open_raster(path, lazy=True)]:
        out = terrio.fill_nodata(src)
        np.testing.assert_array_equal(out.read(1), [[1, 2, 3]])


def test_nearest_search_fringe():
    a = np.full((4, 4), -9999, dtype="float32")
    a[0, 0] = 8
    out = terrio.fill_nodata(raster(a, -9999), interpolation="nearest", max_distance=2)
    np.testing.assert_array_equal(
        out.read(1),
        [[8, 8, 8, -9999], [8, 8, 0, -9999], [8, 0, 0, -9999], [-9999, -9999, -9999, -9999]],
    )


@pytest.mark.parametrize("distance", [0.1, 0.9, 1.1, 1.5, 2.1, 2.9, 4.2, 9.5])
def test_sparse_nearest_smoothing_stays_finite(distance):
    gdal = pytest.importorskip("osgeo.gdal")
    gdal.UseExceptions()
    a = np.zeros((11, 13), dtype="float32")
    a[2, 3], a[8, 10] = 17, 219
    mask = (a != 0).astype("uint8")
    ds = gdal.GetDriverByName("MEM").Create("", 13, 11, 1, gdal.GDT_Float32)
    band = ds.GetRasterBand(1)
    band.WriteArray(a)
    m = gdal.GetDriverByName("MEM").Create("", 13, 11, 1, gdal.GDT_Byte)
    m.GetRasterBand(1).WriteArray(mask)
    gdal.FillNodata(band, m.GetRasterBand(1), distance, 3, ["INTERPOLATION=NEAREST"])
    out = terrio.fill_nodata(
        raster(a),
        mask=raster(mask),
        max_distance=distance,
        interpolation="nearest",
        smoothing_iterations=3,
    ).read(1)
    assert np.isfinite(out).all()
    np.testing.assert_array_equal(out, band.ReadAsArray())
