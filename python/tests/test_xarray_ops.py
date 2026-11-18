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


def test_zonal_stats_plain_geometries_are_in_the_arrays_crs(files):
    """Zones without a CRS of their own (GeoJSON, WKT, bounds) are in the array's CRS, not
    EPSG:4326."""
    da = txr.open_rasterio(files["dem"], masked=True).squeeze("band", drop=True)
    expected = da.terrio.zonal_stats(files["zones"], stats=["mean"]).columns()["raster1_b1_mean"]
    geometries = [f["geometry"] for f in files["zones"].to_geojson()["features"]]
    got = da.terrio.zonal_stats(geometries, stats=["mean"]).columns()["raster1_b1_mean"]
    np.testing.assert_allclose(got, expected)
    first = files["zones"].to_geojson()["features"][0]["geometry"]["coordinates"][0]
    (x0, y1), (x1, _), (_, y0) = first[0], first[1], first[2]
    by_bounds = da.terrio.zonal_stats((x0, y0, x1, y1), stats=["mean"]).columns()
    np.testing.assert_allclose(by_bounds["raster1_b1_mean"], expected[:1])


def test_dataset_zonal_stats_prefixes_variables(files):
    da = txr.open_rasterio(files["dem"], masked=True).squeeze("band", drop=True)
    ds = xr.Dataset({"a": da, "b": da})
    got = ds.terrio.zonal_stats(files["zones"], stats=["mean"]).columns()
    expected = terrio.zonal_stats(
        files["zones"], [files["dem"], files["dem"]], prefixes=["a", "b"], stats=["mean"]
    ).columns()
    assert got.keys() == expected.keys()
    for k in expected:
        np.testing.assert_allclose(got[k], expected[k])


def _same_layer(got, expected):
    assert got.to_wkb() == expected.to_wkb()
    assert got.columns().keys() == expected.columns().keys()
    for k, v in expected.columns().items():
        np.testing.assert_array_equal(got.columns()[k], v)


def test_vector_results(files):
    da = txr.open_rasterio(files["classes"])
    _same_layer(da.terrio.polygonize(band=2), terrio.polygonize(files["classes"], band=2))
    dem = txr.open_rasterio(files["dem"], masked=True).squeeze("band", drop=True)
    _same_layer(dem.terrio.contour(interval=100.0), terrio.contour(files["dem"], interval=100.0))
    _same_layer(dem.terrio.footprint(), terrio.footprint(files["dem"]))


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


def test_band_keyword_picks_a_band_of_a_3d_array(files):
    da = txr.open_rasterio(files["classes"])
    sieved = da.terrio.sieve(band=2, threshold=4)
    assert sieved.dims == ("y", "x")
    expected = terrio.sieve(files["classes"], band=2, threshold=4).read(1)
    np.testing.assert_array_equal(sieved.values, expected)


def test_color_relief_of_a_2d_array_gives_a_band_dimension(files, tmp_path):
    colors = tmp_path / "colors.txt"
    colors.write_text("1000 0 0 0\n2000 255 255 255\n")
    dem = txr.open_rasterio(files["dem"], masked=True).squeeze("band", drop=True)
    relief = dem.terrio.dem("color-relief", color_file=str(colors))
    assert relief.dims == ("band", "y", "x") and relief.sizes["band"] == 3
    expected = terrio.dem(files["dem"], "color-relief", color_file=str(colors)).read()
    np.testing.assert_array_equal(relief.values, expected)


def test_several_bands_per_band_are_refused(files, tmp_path):
    colors = tmp_path / "colors.txt"
    colors.write_text("1000 0 0 0\n2000 255 255 255\n")
    dem = txr.open_rasterio(files["dem"], masked=True).squeeze("band", drop=True)
    with pytest.raises(ValueError, match="3 bands per band"):
        dem.expand_dims(time=2).terrio.dem("color-relief", color_file=str(colors))


def test_viewshed_cropped_to_its_distance(files):
    dem = txr.open_rasterio(files["dem"], masked=True).squeeze("band", drop=True)
    ox, oy = TRANSFORM @ (40.5, 30.5)
    got = dem.terrio.viewshed(ox, oy, md=300.0)
    expected = terrio.viewshed(files["dem"], ox, oy, md=300.0)
    assert got.shape == (expected.height, expected.width)
    assert got.shape != dem.shape
    np.testing.assert_array_equal(got.values, expected.read(1))
    assert got.terrio.transform() == Affine.from_gdal(*expected.transform)
    assert got.terrio.transform(recalc=False) == Affine.from_gdal(*expected.transform)
    assert got.terrio.crs == dem.terrio.crs


def test_spatial_dims_set_by_name(files):
    """Dims named neither x/y nor CF-tagged, set with set_spatial_dims, survive the round trip."""
    da = txr.open_rasterio(files["classes"]).rename(x="col", y="row")
    da.terrio.set_spatial_dims(x_dim="col", y_dim="row", inplace=True)
    sieved = da.terrio.sieve(threshold=4)
    assert sieved.dims == ("band", "row", "col")
    np.testing.assert_array_equal(
        sieved.sel(band=2).values, terrio.sieve(files["classes"], band=2, threshold=4).read(1)
    )
    assert sieved.terrio.x_dim == "col"
    burnt = txr.rasterize(files["zones"], da, attribute="id", init=-1, ot="Int16")
    assert burnt.dims == ("row", "col")


def test_crs_from_attributes_is_kept(files):
    dem = txr.open_rasterio(files["dem"], masked=True).squeeze("band", drop=True)
    bare = dem.drop_vars("spatial_ref").assign_attrs(crs="EPSG:32611")
    assert bare.terrio.crs == dem.terrio.crs
    assert bare.terrio.dem("slope").terrio.crs == dem.terrio.crs


def test_columns_from_east_to_west_give_the_west_to_east_result(files):
    """A grid whose columns run east to west is flipped for terrio and back, as rows are."""
    dem = txr.open_rasterio(files["dem"], masked=True).squeeze("band", drop=True)
    reversed_dem = dem.isel(x=slice(None, None, -1))
    got = reversed_dem.terrio.dem("slope")
    expected = dem.terrio.dem("slope")
    assert got.dims == ("y", "x")
    np.testing.assert_array_equal(got.x.values, expected.x.values[::-1])
    np.testing.assert_array_equal(got.values, expected.values[:, ::-1])


def test_rasterize_on_a_single_row(files):
    da = txr.open_rasterio(files["dem"]).squeeze("band", drop=True)
    row = da.isel(y=slice(55, 56))
    burnt = txr.rasterize(files["zones"], row, attribute="id", init=-1, ot="Int16")
    expected = terrio.rasterize(
        files["zones"], like=files["dem"], attribute="id", init=-1, ot="Int16"
    ).read(1)[55:56]
    np.testing.assert_array_equal(burnt.values, expected)
