"""terrio's raster operations on the `.terrio` accessor give what the terrio functions give."""

import numpy as np
import pytest

xr = pytest.importorskip("xarray")
pytest.importorskip("dask")

from affine import Affine  # noqa: E402

import terrio  # noqa: E402
import terrio.rio as rio  # noqa: E402
import terrio.xarray as txr  # noqa: E402

TRANSFORM = Affine(30.0, 0.0, 500000.0, 0.0, -30.0, 4100000.0)
H, W = 120, 160


@pytest.fixture(scope="module")
def files(tmp_path_factory):
    d = tmp_path_factory.mktemp("xarray_ops")
    yy, xx = np.mgrid[0:H, 0:W]
    dem = (np.sin(xx / 23.0) * np.cos(yy / 17.0) * 300 + 1500).astype("float32")
    dem[50:60, 70:90] = -9999
    profile = dict(
        driver="GTiff", width=W, height=H, count=1, dtype="float32", crs="EPSG:32611",
        transform=TRANSFORM, nodata=-9999,
    )  # fmt: skip
    with rio.open(d / "dem.tif", "w", **profile) as dst:
        dst.write(dem, 1)
    classes = np.stack([(xx // 7 + yy // 9 + b) % 4 for b in range(2)]).astype("uint8")
    classes[:, ::13, ::11] = 9  # single pixels for the sieve
    profile.update(count=2, dtype="uint8", nodata=255)
    with rio.open(d / "classes.tif", "w", **profile) as dst:
        dst.write(classes)
    x0, y0 = TRANSFORM.c, TRANSFORM.f
    zones = {
        "type": "FeatureCollection",
        "features": [
            {
                "type": "Feature",
                "properties": {"id": i},
                "geometry": {
                    "type": "Polygon",
                    "coordinates": [
                        [
                            [x0 + a, y0 - b],
                            [x0 + a + 1500, y0 - b],
                            [x0 + a + 1500, y0 - b - 1200],
                            [x0 + a, y0 - b - 1200],
                            [x0 + a, y0 - b],
                        ]
                    ],
                },
            }
            for i, (a, b) in enumerate([(100, 100), (2000, 1500), (3000, 400)])
        ],
    }
    layer = terrio.Layer.from_geojson(zones, crs="EPSG:32611")
    return {"dem": str(d / "dem.tif"), "classes": str(d / "classes.tif"), "zones": layer}


def _masked(raster, band=1):
    values = raster.read(band).astype("float64")
    if raster.nodata is not None:
        values[values == raster.nodata] = np.nan
    return values


@pytest.mark.parametrize("chunks", [None, 64])
def test_dem_matches_terrio(files, chunks):
    da = txr.open_rasterio(files["dem"], masked=True, chunks=chunks).squeeze("band", drop=True)
    slope = da.terrio.dem("slope", scale=1.0)
    expected = _masked(terrio.dem(files["dem"], "slope", scale=1.0))
    np.testing.assert_array_equal(slope.values, expected)
    assert slope.dims == da.dims
    assert slope.terrio.crs == da.terrio.crs
    assert slope.terrio.transform() == da.terrio.transform()
    assert np.isnan(slope.terrio.nodata) and slope.encoding["_FillValue"] == -9999


def test_south_up_array_comes_back_south_up(files):
    da = txr.open_rasterio(files["dem"], masked=True).squeeze("band", drop=True)
    flipped = da.isel(y=slice(None, None, -1))
    hillshade = flipped.terrio.dem("hillshade")
    np.testing.assert_array_equal(hillshade.values[::-1], da.terrio.dem("hillshade").values)
    np.testing.assert_array_equal(hillshade.y.values, flipped.y.values)


def test_bands_one_by_one(files):
    da = txr.open_rasterio(files["classes"])
    sieved = da.terrio.sieve(threshold=4)
    assert sieved.dims == da.dims and sieved.dtype == da.dtype
    for b in (1, 2):
        expected = terrio.sieve(files["classes"], band=b, threshold=4).read(1)
        np.testing.assert_array_equal(sieved.sel(band=b).values, expected)
    assert sieved.attrs["_FillValue"] == 255


def test_dataset_runs_each_variable(files):
    ds = xr.open_dataset(files["dem"], engine="terrio", mask_and_scale=True)
    out = ds.terrio.dem("aspect")
    np.testing.assert_array_equal(
        out["band_data"].squeeze("band").values, _masked(terrio.dem(files["dem"], "aspect"))
    )


def test_zonal_stats_matches_terrio(files):
    da = txr.open_rasterio(files["dem"], masked=True).squeeze("band", drop=True)
    stats = ["count", "mean", "max"]
    got = da.terrio.zonal_stats(files["zones"], stats=stats).columns()
    expected = terrio.zonal_stats(files["zones"], files["dem"], stats=stats).columns()
    assert got.keys() == expected.keys()
    for k in expected:
        np.testing.assert_allclose(got[k], expected[k])


def test_dataset_zonal_stats_prefixes_variables(files):
    ds = xr.Dataset(
        {
            "a": txr.open_rasterio(files["dem"], masked=True).squeeze("band", drop=True),
            "b": txr.open_rasterio(files["dem"], masked=True).squeeze("band", drop=True) * 2,
        }
    )
    cols = ds.terrio.zonal_stats(files["zones"], stats=["mean"]).columns()
    np.testing.assert_allclose(cols["b_b1_mean"], 2 * np.asarray(cols["a_b1_mean"]))


def test_vector_results(files):
    da = txr.open_rasterio(files["classes"]).sel(band=1)
    assert len(da.terrio.polygonize()) == len(terrio.polygonize(files["classes"]))
    dem = txr.open_rasterio(files["dem"], masked=True).squeeze("band", drop=True)
    assert len(dem.terrio.contour(interval=100.0)) == len(
        terrio.contour(files["dem"], interval=100.0)
    )
    assert len(dem.terrio.footprint()) == len(terrio.footprint(files["dem"]))


def test_raster_ops_match_terrio(files):
    da = txr.open_rasterio(files["classes"]).sel(band=1)
    np.testing.assert_array_equal(
        da.terrio.proximity(values=[0]).values,
        terrio.proximity(files["classes"], values=[0]).read(1),
    )
    np.testing.assert_array_equal(
        da.terrio.reclassify("[0,1]=10; DEFAULT=20").values,
        terrio.reclassify(files["classes"], "[0,1]=10; DEFAULT=20").read(1),
    )
    dem = txr.open_rasterio(files["dem"], masked=True).squeeze("band", drop=True)
    np.testing.assert_array_equal(
        dem.terrio.fill_nodata().values, _masked(terrio.fill_nodata(files["dem"]))
    )
    ox, oy = TRANSFORM @ (40.5, 30.5)
    np.testing.assert_array_equal(
        dem.terrio.viewshed(ox, oy).values, terrio.viewshed(files["dem"], ox, oy).read(1)
    )


def test_rasterize_on_a_grid(files):
    da = txr.open_rasterio(files["dem"], chunks=64).squeeze("band", drop=True)
    burnt = txr.rasterize(files["zones"], da, attribute="id", init=-1, ot="Int16")
    expected = terrio.rasterize(
        files["zones"], like=files["dem"], attribute="id", init=-1, ot="Int16"
    )
    np.testing.assert_array_equal(burnt.values, expected.read(1))
    assert burnt.dims == ("y", "x") and burnt.terrio.crs == da.terrio.crs
    np.testing.assert_array_equal(burnt.x.values, da.x.values)


def test_four_dimensions_are_refused(files):
    da = txr.open_rasterio(files["classes"]).expand_dims(time=2)
    with pytest.raises(txr.exceptions.TooManyDimensions):
        da.terrio.sieve()
