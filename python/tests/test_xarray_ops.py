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


def test_flipped_array_runs_without_affine_matmul(files, monkeypatch):
    # affine 2, which the requirement allows, has no `@` for transforms.
    da = txr.open_rasterio(files["dem"], masked=True).squeeze("band", drop=True)
    expected = da.terrio.dem("hillshade").values
    monkeypatch.delattr(Affine, "__matmul__", raising=False)
    monkeypatch.delattr(Affine, "__rmatmul__", raising=False)
    flipped = da.isel(y=slice(None, None, -1), x=slice(None, None, -1))
    hillshade = flipped.terrio.dem("hillshade")
    np.testing.assert_array_equal(hillshade.values[::-1, ::-1], expected)


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


@pytest.fixture
def cube(files):
    dem = txr.open_rasterio(files["dem"], masked=True).squeeze("band", drop=True)
    t = xr.DataArray(np.array(["2024-01-01", "2024-02-01", "2024-03-01"], "M8[ns]"), dims="time")
    return xr.concat([dem, dem * 2, dem + 100], dim=t)


@pytest.mark.parametrize("chunks", [None, {"time": 1, "x": 50}])
def test_zonal_stats_of_a_cube_matches_each_slice(files, cube, chunks):
    """A cube gives `(geometry, ..., statistic)`: the zones' Shapely geometries in their
    CRS, their attributes along `geometry`, each slice as terrio.zonal_stats gives it."""
    shapely = pytest.importorskip("shapely")
    stats = ["count", "mean", "median"]
    out = (cube.chunk(chunks) if chunks else cube).terrio.zonal_stats(files["zones"], stats=stats)
    assert out.dims == ("geometry", "time", "statistic")
    assert out.dtype == np.float64
    assert out.statistic.values.tolist() == stats
    assert out.id.dims == ("geometry",) and out.id.values.tolist() == [0, 1, 2]
    assert out.terrio.crs == "EPSG:32611"
    expected_geometries = shapely.from_wkb(files["zones"].to_wkb())
    assert shapely.equals(out.geometry.values, expected_geometries).all()
    for i in range(cube.sizes["time"]):
        columns = cube.isel(time=i).terrio.zonal_stats(files["zones"], stats=stats).columns()
        for s in stats:
            expected = np.array([np.nan if v is None else v for v in columns[f"raster1_b1_{s}"]])
            np.testing.assert_array_equal(out.isel(time=i).sel(statistic=s).values, expected)


@pytest.mark.parametrize("chunks", [None, {"time": 1}, {"band": 1, "time": 2, "y": 7}])
def test_zonal_stats_of_a_cube_in_batches_and_strips_equals_one_pass(
    files, cube, chunks, monkeypatch
):
    """Batches of one chunk (`batch_bytes=1` warns), strips of 3 rows and variables read
    together give what one pass gives."""
    pytest.importorskip("shapely")
    from terrio.xarray import _zonal

    four = xr.concat([cube, cube + 1], dim="band").assign_coords(band=["red", "nir"])
    four = four.chunk(chunks) if chunks else four
    stats = ["count", "mean", "median", "stddev"]
    whole = four.terrio.zonal_stats(files["zones"], stats=stats)
    assert whole.dims == ("geometry", "band", "time", "statistic")
    with pytest.warns(RuntimeWarning, match="more than batch_bytes") as caught:
        one = four.terrio.zonal_stats(files["zones"], stats=stats, batch_bytes=1)
    assert caught[0].filename == __file__  # points at the call
    xr.testing.assert_identical(one, whole)
    monkeypatch.setattr(_zonal, "_STRIP_PIXELS", 3 * W)
    xr.testing.assert_identical(four.terrio.zonal_stats(files["zones"], stats=stats), whole)
    ds = xr.Dataset({"a": four, "b": four * 3, "dem": cube.isel(time=0)})
    together = ds.terrio.zonal_stats(files["zones"], stats=stats)
    with pytest.warns(RuntimeWarning):
        apart = ds.terrio.zonal_stats(files["zones"], stats=stats, batch_bytes=1)
    for out in (together, apart):
        xr.testing.assert_identical(out["a"], whole.rename("a"))
        xr.testing.assert_identical(
            out["dem"], ds["dem"].expand_dims(t=1).terrio.zonal_stats(files["zones"], stats=stats)
            .isel(t=0, drop=True).rename("dem")
        )  # fmt: skip


@pytest.mark.parametrize("chunks", [None, {"time": 1, "y": 7, "x": 50}])
@pytest.mark.parametrize(
    "flip",
    [
        {"y": slice(None, None, -1)},
        {"x": slice(None, None, -1)},
        {"y": slice(None, None, -1), "x": slice(None, None, -1)},
    ],
    ids=["south-up", "east-to-west", "both"],
)
def test_zonal_stats_of_a_flipped_cube_equals_the_north_up_one(files, cube, chunks, flip):
    """A cube whose rows run south-up or whose columns run east to west is read north-up,
    lazily or in dask chunks, and gives the north-up result."""
    pytest.importorskip("shapely")
    stats = ["count", "mean", "median"]
    north = cube.chunk(chunks) if chunks else cube
    expected = north.terrio.zonal_stats(files["zones"], stats=stats)
    got = north.isel(flip).terrio.zonal_stats(files["zones"], stats=stats)
    xr.testing.assert_identical(got, expected)


@pytest.mark.parametrize(
    ("y_chunks", "strip_rows"),
    [(7, 3), (2, 5), ((4, 6, 10, 100), 5), ((30, 45, 45), 50), ((0, 0, H), 3), ((0, H, 0), 3)],
    ids=["strip-shorter", "strip-taller", "uneven", "uneven-taller", "zero-leading", "zero-ends"],
)
def test_zonal_stats_of_a_cube_computes_each_dask_chunk_once(
    files, cube, monkeypatch, y_chunks, strip_rows
):
    """Strips shorter or taller than the chunks, on equal or uneven chunks: a strip across a
    chunk edge would compute the next chunk once for it and once more for its own strips."""
    pytest.importorskip("shapely")
    from terrio.xarray import _zonal

    calls = {}

    def count(block, block_info=None):
        loc = tuple(block_info[0]["chunk-location"])
        calls[loc] = calls.get(loc, 0) + 1
        return block

    data = cube.chunk(time=1, y=y_chunks)
    expected = cube.terrio.zonal_stats(files["zones"], stats=["mean"])
    data = data.copy(data=data.data.map_blocks(count, dtype=data.dtype))
    monkeypatch.setattr(_zonal, "_STRIP_PIXELS", strip_rows * W)
    got = data.terrio.zonal_stats(files["zones"], stats=["mean"])
    assert calls and max(calls.values()) == 1, calls
    xr.testing.assert_identical(got, expected)


def test_zonal_stats_of_a_cube_selects_the_layer_of_any_zones_input(files, cube, tmp_path):
    """`layer=` picks a layer of a VectorDataset as of a path, and names the layer a `Layer`
    must be."""
    pytest.importorskip("shapely")
    zones = terrio.Layer.from_geojson(files["zones"].to_geojson(), name="polygons", crs="EPSG:32611")
    expected = cube.terrio.zonal_stats(zones, stats=["mean"])
    path = str(tmp_path / "zones.gpkg")
    terrio.write_vector(zones, path)
    for src in (zones, path, terrio.open_vector(path)):
        out = cube.terrio.zonal_stats(src, stats=["mean"], layer="polygons")
        np.testing.assert_array_equal(out.values, expected.values)
    with pytest.raises(ValueError, match="no layer named 'roads'"):
        cube.terrio.zonal_stats(zones, stats=["mean"], layer="roads")
    with pytest.raises(ValueError, match="layer= applies to"):
        cube.terrio.zonal_stats(zones.to_geojson(), stats=["mean"], layer="polygons")


def test_zonal_stats_of_a_cube_refuses_2d_only_options_by_name(files, cube):
    """Options of 2-D arrays only (bands, weights...) name themselves and the cube's options."""
    with pytest.raises(TypeError, match="bands: not available on arrays with dimensions"):
        cube.terrio.zonal_stats(files["zones"], bands=[1])


def test_zonal_stats_of_a_cube_refuses_a_batch_below_one_byte(files, cube):
    """`batch_bytes` below one byte would hold no batch at all: it is refused by name."""
    with pytest.raises(ValueError, match="batch_bytes must be at least 1 byte"):
        cube.terrio.zonal_stats(files["zones"], batch_bytes=0)


def test_zonal_stats_of_a_cube_warns_for_its_largest_chunk(files, cube):
    """Uneven chunks along the first other dimension: the largest one is checked against
    `batch_bytes`, not the first."""
    pytest.importorskip("shapely")
    uneven = xr.concat([cube] * 4, dim="time").chunk(time=(1, 11))
    one_slice = H * W * uneven.dtype.itemsize
    with pytest.warns(RuntimeWarning, match="more than batch_bytes"):
        uneven.terrio.zonal_stats(files["zones"], stats=["mean"], batch_bytes=3 * one_slice)


def test_zonal_stats_of_a_cube_suffixes_attributes_named_like_coordinates(files, cube):
    """A zone attribute named like a coordinate of the result (`time`, `statistic`) is kept,
    with a suffix."""
    pytest.importorskip("shapely")
    zones = terrio.Layer.from_geojson(
        {
            "type": "FeatureCollection",
            "features": [
                {**f, "properties": {**f["properties"], "time": "spring", "statistic": 7}}
                for f in files["zones"].to_geojson()["features"]
            ],
        },
        crs="EPSG:32611",
    )
    out = cube.terrio.zonal_stats(zones, stats=["mean"])
    assert out.dims == ("geometry", "time", "statistic")
    assert out["time_2"].dims == ("geometry",) and set(out["time_2"].values) == {"spring"}
    assert out["statistic_2"].values.tolist() == [7] * out.sizes["geometry"]
    np.testing.assert_array_equal(out.time.values, cube.time.values)


def test_zonal_stats_of_a_cube_without_zone_pixels(cube):
    """Zones off the grid, and an empty time axis, give NaN and empty cubes."""
    pytest.importorskip("shapely")
    off = cube.terrio.zonal_stats((0.0, 0.0, 1.0, 1.0), stats=["count", "mean"])
    assert off.sizes == {"geometry": 1, "time": 3, "statistic": 2}
    np.testing.assert_array_equal(off.sel(statistic="count").values, 0)
    assert np.isnan(off.sel(statistic="mean").values).all()
    empty = cube.isel(time=slice(0, 0)).terrio.zonal_stats((0.0, 0.0, 1.0, 1.0), stats=["mean"])
    assert empty.sizes["time"] == 0


def _zones_with_property(files, name, value):
    """The zones with one more property, as a Layer in EPSG:32611."""
    return terrio.Layer.from_geojson(
        {
            "type": "FeatureCollection",
            "features": [
                {**f, "properties": {**f["properties"], name: value}}
                for f in files["zones"].to_geojson()["features"]
            ],
        },
        crs="EPSG:32611",
    )


def test_zonal_stats_of_a_cube_suffixes_an_attribute_named_like_the_grid_mapping(files, cube):
    """A zone attribute named `spatial_ref` is kept with a suffix: the grid mapping coordinate
    still holds the CRS."""
    pytest.importorskip("shapely")
    zones = _zones_with_property(files, "spatial_ref", "plot")
    out = cube.terrio.zonal_stats(zones, stats=["mean"])
    assert out.terrio.crs == "EPSG:32611"
    assert set(out["spatial_ref_2"].values) == {"plot"}


def test_zonal_stats_of_a_dataset_names_attributes_alike_for_every_variable(files, cube):
    """A 2-D variable and a cube of one Dataset name a zone attribute clashing with the cube's
    `time` dimension alike, so the variables assemble."""
    pytest.importorskip("shapely")
    zones = _zones_with_property(files, "time", "spring")
    ds = xr.Dataset({"a": cube, "dem": cube.isel(time=0, drop=True)})
    out = ds.terrio.zonal_stats(zones, stats=["mean"])
    assert out["a"]["time_2"].dims == ("geometry",)
    assert out["dem"]["time_2"].dims == ("geometry",)
    assert "time" not in out["dem"].coords


def test_zonal_stats_reserves_dataset_variable_names(files, cube):
    """Keep a zone attribute whose name matches another result variable."""
    pytest.importorskip("shapely")
    zones = _zones_with_property(files, "b", "plot")
    ds = xr.Dataset({"a": cube, "b": cube * 2})
    out = ds.terrio.zonal_stats(zones, stats=["mean"])
    assert set(out.data_vars) == {"a", "b"}
    assert set(out.b_2.values) == {"plot"}
    np.testing.assert_array_equal(out.b.values, out.a.values * 2)


def test_zonal_stats_preserves_counts_above_float32_precision():
    """Return an exact count above 2**24 from real float32 pixels."""
    pytest.importorskip("shapely")
    height, width = 4097, 4097
    data = xr.DataArray(
        np.broadcast_to(np.float32(1), (1, height, width)),
        dims=("time", "y", "x"),
        coords={"x": np.arange(width) + 0.5, "y": height - np.arange(height) - 0.5},
    ).terrio.write_crs("EPSG:3857")
    out = data.terrio.zonal_stats((0, 0, width, height), stats=["count", "mean"])
    assert out.dtype == np.float64
    assert out.sel(statistic="count").item() == height * width
    assert out.sel(statistic="mean").item() == 1


@pytest.mark.parametrize("fractional", [False, True])
def test_zonal_stats_budgets_complete_overlapping_zones(files, cube, monkeypatch, fractional):
    """Limit concurrent streams using complete retained zones, across many strips."""
    pytest.importorskip("shapely")
    from terrio.xarray import _zonal

    monkeypatch.setattr(_zonal, "_STRIP_PIXELS", 3 * W)
    bounds = cube.terrio.bounds()
    zone = terrio.Layer.from_geojson(
        {
            "type": "FeatureCollection",
            "features": [
                {
                    "type": "Feature",
                    "properties": {},
                    "geometry": {
                        "type": "Polygon",
                        "coordinates": [
                            [
                                [bounds[0], bounds[1]],
                                [bounds[2], bounds[1]],
                                [bounds[2], bounds[3]],
                                [bounds[0], bounds[3]],
                                [bounds[0], bounds[1]],
                            ]
                        ],
                    },
                }
                for _ in range(2)
            ],
        },
        crs="EPSG:32611",
    )
    expected = cube.terrio.zonal_stats(zone, stats=["median"], fractional=fractional)
    real = _zonal._terrio.ZonalReducer
    live, peak = 0, 0

    class Stream:
        """Count simultaneously retained streams around the real reducer."""

        def __init__(self, wrapped):
            """Record a live stream.

            Args:
                wrapped: The native stream to delegate to.
            """
            nonlocal live, peak
            self.wrapped = wrapped
            live += 1
            peak = max(peak, live)

        def add(self, *args):
            """Reduce a window.

            Args:
                *args: Native window arguments.
            """
            return self.wrapped.add(*args)

        def finish(self):
            """Finish the stream and release its retained values.

            Returns:
                The native statistic columns.
            """
            nonlocal live
            live -= 1
            return self.wrapped.finish()

    class Reducer:
        """Delegate native reduction while observing peak stream count."""

        def __init__(self, *args, **kwargs):
            """Create the native reducer.

            Args:
                *args: Native positional arguments.
                **kwargs: Native keyword arguments.
            """
            self.wrapped = real(*args, **kwargs)

        def __getattr__(self, name):
            """Read native metadata.

            Args:
                name: Attribute name.

            Returns:
                The native attribute value.
            """
            return getattr(self.wrapped, name)

        def stream(self, fill):
            """Observe one new native stream.

            Args:
                fill: The nodata value.

            Returns:
                A counted stream.
            """
            return Stream(self.wrapped.stream(fill))

    monkeypatch.setattr(_zonal._terrio, "ZonalReducer", Reducer)
    # One full pair of zones fits, two do not. The old two-strip estimate packs all times.
    held = H * W * 2 * 32
    got = cube.terrio.zonal_stats(
        zone, stats=["median"], fractional=fractional, batch_bytes=held + 3 * W * 4
    )
    assert peak == 1
    xr.testing.assert_identical(got, expected)


def test_zonal_stats_budgets_2d_variables_in_mixed_datasets(files, cube, monkeypatch):
    """Pack spatial-only variables separately when their reads exceed the budget."""
    pytest.importorskip("shapely")
    import dask

    data = cube.chunk(time=1, y=H, x=W)
    ds = xr.Dataset({"cube": data, **{f"v{i}": data.isel(time=0, drop=True) + i for i in range(8)}})
    expected = ds.terrio.zonal_stats(files["zones"], stats=["mean"])
    real = dask.compute
    calls = []

    def compute(*args, **kwargs):
        """Observe the bytes requested together.

        Args:
            *args: Dask arrays.
            **kwargs: Compute options.

        Returns:
            Computed arrays.
        """
        calls.append(sum(a.nbytes for a in args))
        return real(*args, **kwargs)

    monkeypatch.setattr(dask, "compute", compute)
    budget = 2 * H * W * data.dtype.itemsize
    got = ds.terrio.zonal_stats(files["zones"], stats=["mean"], batch_bytes=budget)
    assert calls and max(calls) <= budget
    xr.testing.assert_identical(got, expected)
    with pytest.warns(RuntimeWarning, match="more than batch_bytes"):
        ds.terrio.zonal_stats(files["zones"], stats=["mean"], batch_bytes=1)


def test_zonal_stats_reads_only_lazy_zone_windows(files, monkeypatch, record_property):
    """Measure decoded bytes from the lazy backend for a zone covering part of a grid."""
    pytest.importorskip("shapely")
    from terrio.xarray import _io, _zonal

    real = _io._Source.read
    reads = []

    def read(source, bands, window):
        """Measure a native window read.

        Args:
            source: The lazy raster source.
            bands: The requested bands.
            window: Requested row and column intervals.

        Returns:
            The native pixel array.
        """
        values = real(source, bands, window)
        reads.append(values.nbytes)
        return values

    monkeypatch.setattr(_io._Source, "read", read)
    monkeypatch.setattr(_zonal, "_STRIP_PIXELS", 3 * W)
    data = txr.open_rasterio(files["dem"])
    # Keep the length-one band dimension so it uses the cube reducer, with no eager concat.
    bounds = (500300, 4099400, 500600, 4099700)
    out = data.terrio.zonal_stats(bounds, stats=["mean"])
    assert np.isfinite(out.values).all()
    record_property("decoded_bytes", sum(reads))
    record_property("full_grid_bytes", H * W * data.dtype.itemsize)
    record_property("window_reads", len(reads))
    dx, dy = map(abs, data.terrio.resolution())
    zone_pixels = int((bounds[2] - bounds[0]) / dx) * int((bounds[3] - bounds[1]) / dy)
    assert reads and sum(reads) == zone_pixels * data.dtype.itemsize
    assert sum(reads) < H * W * data.dtype.itemsize


def test_zonal_stats_uses_the_active_geodataframe_geometry(files, cube):
    """Use a renamed active geometry after a secondary geometry, preserving attributes."""
    gpd = pytest.importorskip("geopandas")
    pytest.importorskip("pyarrow")
    shapely = pytest.importorskip("shapely")
    frame = files["zones"].to_geopandas().rename_geometry("outline")
    frame.insert(
        0, "secondary", gpd.GeoSeries([shapely.box(0, 0, 1, 1)] * len(frame), crs=frame.crs)
    )
    expected = cube.terrio.zonal_stats(files["zones"], stats=["mean"])
    got = cube.terrio.zonal_stats(frame, stats=["mean"])
    xr.testing.assert_identical(got, expected)


@pytest.mark.parametrize("name", ["geometry", "statistic"])
@pytest.mark.parametrize("dimension", [False, True])
def test_zonal_stats_refuses_reserved_input_names(files, cube, name, dimension):
    """Name collisions fail explicitly rather than losing an input coordinate."""
    pytest.importorskip("shapely")
    data = cube.rename(time=name) if dimension else cube.assign_coords({name: ("time", [1, 2, 3])})
    with pytest.raises(ValueError, match="reserves result names"):
        data.terrio.zonal_stats(files["zones"], stats=["mean"])
