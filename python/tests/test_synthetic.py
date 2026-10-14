"""Tests on small data made in the tests, that always run."""

import json

import numpy as np
import pytest

import terrio

TRANSFORM = (500000.0, 30.0, 0.0, 4100000.0, 0.0, -30.0)

POLYGONS = {
    "type": "FeatureCollection",
    "features": [
        {
            "type": "Feature",
            "properties": {"name": "west", "value": 3, "area": 1.5},
            "geometry": {
                "type": "Polygon",
                "coordinates": [[[0, 0], [10, 0], [10, 10], [0, 10], [0, 0]]],
            },
        },
        {
            "type": "Feature",
            "properties": {"name": "east", "value": 7, "area": None},
            "geometry": {
                "type": "Polygon",
                "coordinates": [[[20, 0], [30, 0], [30, 10], [20, 10], [20, 0]]],
            },
        },
    ],
}


def make_raster(dtype="int16", bands=2, nodata=-1):
    data = np.arange(bands * 6 * 8, dtype=dtype).reshape(bands, 6, 8)
    return data, terrio.Raster.from_array(data, TRANSFORM, crs="EPSG:32610", nodata=nodata)


def test_version():
    assert isinstance(terrio.__version__, str)


@pytest.mark.parametrize(
    "dtype", ["uint8", "int8", "uint16", "int16", "uint32", "int32", "int64", "float32", "float64"]
)
def test_from_array_properties(dtype):
    data, r = make_raster(dtype)
    assert (r.count, r.height, r.width) == (2, 6, 8)
    assert r.shape == data.shape
    assert r.dtype == dtype
    assert r.nodata == -1
    assert r.crs == "EPSG:32610"
    assert "UTM zone 10N" in r.crs_wkt
    assert r.transform == TRANSFORM
    assert r.res == (30.0, 30.0)
    assert r.bounds == (500000.0, 4100000.0 - 6 * 30, 500000.0 + 8 * 30, 4100000.0)
    assert not r.is_lazy
    out = r.read()
    assert out.dtype == np.dtype(dtype)
    np.testing.assert_array_equal(out, data)
    np.testing.assert_array_equal(np.asarray(r), data)


def test_read_bands_and_windows():
    data, r = make_raster()
    np.testing.assert_array_equal(r.read(2), data[1])
    np.testing.assert_array_equal(r.read([2, 1]), data[[1, 0]])
    np.testing.assert_array_equal(r.read(1, window=(2, 1, 3, 4)), data[0, 1:5, 2:5])
    with pytest.raises(terrio.InvalidArgumentError):
        r.read(3)
    with pytest.raises(ValueError):
        r.read(window=(6, 0, 4, 1))
    with pytest.raises(terrio.InvalidArgumentError):
        r.read(overview=0)


def test_from_array_2d_and_bool():
    r = terrio.Raster.from_array(np.ones((3, 4), dtype=bool), TRANSFORM)
    assert r.count == 1
    assert r.dtype == "uint8"
    assert r.crs is None
    assert r.nodata is None
    assert r.read(1).sum() == 12


def test_from_array_rejects_bad_input():
    with pytest.raises(terrio.InvalidArgumentError):
        terrio.Raster.from_array(np.zeros(5), TRANSFORM)
    with pytest.raises(terrio.InvalidArgumentError):
        terrio.Raster.from_array(np.zeros((2, 2)), (0, 1, 0.5, 0, 0, -1))
    with pytest.raises(terrio.InvalidArgumentError):
        terrio.Raster.from_array(np.zeros((2, 2), dtype=np.complex64), TRANSFORM)
    with pytest.raises(terrio.CRSError):
        terrio.Raster.from_array(np.zeros((2, 2)), TRANSFORM, crs="not a crs")


@pytest.mark.parametrize("ext", ["tif", "asc"])
def test_write_and_reopen(tmp_path, ext):
    data, r = make_raster("float32", bands=1 if ext == "asc" else 2)
    path = tmp_path / f"out.{ext}"
    r.write(str(path))
    back = terrio.open(str(path))
    assert isinstance(back, terrio.Raster)
    np.testing.assert_allclose(back.read(), data)
    assert back.transform == pytest.approx(TRANSFORM)


def test_write_compressed_tiled_and_lazy(tmp_path):
    data, r = make_raster("uint16")
    path = tmp_path / "tiled.tif"
    terrio.write_raster(r, str(path), compress="DEFLATE", tile_size=16)
    lazy = terrio.open_raster(str(path), lazy=True)
    assert lazy.is_lazy
    assert lazy.nodata == -1 or lazy.nodata is not None
    np.testing.assert_array_equal(lazy.read(2, window=(1, 1, 3, 2)), data[1, 1:3, 1:4])
    assert "lazy" in repr(lazy)


def test_bytes_round_trip():
    data, r = make_raster("int32")
    tif = r.to_bytes("x.tif")
    assert tif[:4] in (b"II*\x00", b"MM\x00*")
    back = terrio.open_bytes(tif, "x.tif")
    np.testing.assert_array_equal(back.read(), data)
    lazy = terrio.open_bytes(tif, "x.tif", lazy=True)
    assert lazy.is_lazy
    np.testing.assert_array_equal(lazy.read(1, window=(0, 0, 2, 2)), data[0, :2, :2])


def test_vector_from_geojson_and_back():
    layer = terrio.Layer.from_geojson(POLYGONS, name="squares")
    assert layer.name == "squares"
    assert len(layer) == 2
    assert layer.crs == "EPSG:4326"
    assert layer.geometry_type == "Polygon"
    assert layer.fields == {"name": "Text", "value": "Integer", "area": "Float"}
    assert layer.bounds == (0, 0, 30, 10)
    cols = layer.columns()
    assert cols["name"] == ["west", "east"]
    assert cols["area"] == [1.5, None]
    fc = layer.to_geojson()
    assert fc["type"] == "FeatureCollection"
    assert [f["properties"]["value"] for f in fc["features"]] == [3, 7]
    assert layer.__geo_interface__["type"] == "FeatureCollection"
    wkb = layer.to_wkb()
    assert len(wkb) == 2 and all(isinstance(w, bytes) for w in wkb)
    # From a JSON string too.
    assert len(terrio.Layer.from_geojson(json.dumps(POLYGONS))) == 2
    layer.name = "renamed"
    assert layer.name == "renamed"


@pytest.mark.parametrize("ext", ["geojson", "fgb", "gpkg", "kml"])
def test_write_vector_formats(tmp_path, ext):
    path = tmp_path / f"squares.{ext}"
    terrio.write_vector(POLYGONS, str(path))
    ds = terrio.open_vector(str(path))
    assert isinstance(ds, terrio.VectorDataset)
    layer = ds[0]
    assert len(layer) == 2
    assert layer.name == "squares"
    assert terrio.list_layers(str(path)) == ["squares"]


def test_vector_dataset_access(tmp_path):
    a = terrio.Layer.from_geojson(POLYGONS, name="a")
    b = terrio.Layer.from_geojson(POLYGONS, name="b")
    path = tmp_path / "two.gpkg"
    terrio.write_vector([a, b], str(path))
    ds = terrio.open_vector(str(path))
    assert ds.layer_names == ["a", "b"]
    assert ds.driver == "GPKG"
    assert len(ds) == 2
    assert ds["b"].name == "b"
    assert ds[-1].name == "b"
    assert [layer.name for layer in ds] == ["a", "b"]
    assert [layer.name for layer in ds.layers] == ["a", "b"]
    with pytest.raises(KeyError):
        ds["c"]
    with pytest.raises(IndexError):
        ds[2]
    only_b = terrio.open_vector(str(path), layers=["b"])
    assert only_b.layer_names == ["b"]
    with pytest.raises(terrio.InvalidArgumentError):
        terrio.write_vector(ds, str(tmp_path / "two.geojson"))


def test_layer_bytes_and_to_crs():
    layer = terrio.Layer.from_geojson(POLYGONS, name="squares")
    fgb = layer.to_bytes("squares.fgb")
    back = terrio.open_bytes(fgb, "squares.fgb")
    assert len(back[0]) == 2
    projected = layer.to_crs("EPSG:3857")
    assert projected.crs == "EPSG:3857"
    assert projected.bounds[2] == pytest.approx(30 * 20037508.342789244 / 180)


def test_convert_with_filters(tmp_path):
    src = tmp_path / "squares.geojson"
    terrio.write_vector(POLYGONS, str(src))
    dst = tmp_path / "east.gpkg"
    terrio.convert(str(src), str(dst), where="value > 5", select=["name"], t_srs="EPSG:32631")
    layer = terrio.open_vector(str(dst))[0]
    assert len(layer) == 1
    assert layer.fields == {"name": "Text"}
    assert layer.crs == "EPSG:32631"
    with pytest.raises(terrio.InvalidArgumentError):
        terrio.convert(str(src), str(tmp_path / "x.fgb"), nlt="NOT_A_TYPE")


def test_convert_raster_window(tmp_path):
    data, r = make_raster("uint8")
    src = tmp_path / "src.tif"
    r.write(str(src))
    dst = tmp_path / "win.tif"
    terrio.convert(str(src), str(dst), srcwin=(2, 1, 4, 3))
    np.testing.assert_array_equal(terrio.open_raster(str(dst)).read(), data[:, 1:4, 2:6])
    with pytest.raises(terrio.InvalidArgumentError):
        terrio.convert(str(src), str(tmp_path / "x.tif"), where="a = 1")


def test_rasterize_and_polygonize(tmp_path):
    layer = terrio.Layer.from_geojson(POLYGONS, name="squares", crs="EPSG:32610")
    r = terrio.rasterize(layer, attribute="value", tr=(1, 1), ot="Byte")
    # As gdal_rasterize -tr 1 1 makes it.
    assert (r.width, r.height) == (31, 11)
    a = r.read(1)
    assert a.dtype == np.uint8
    assert (a == 3).sum() == (a == 7).sum() > 0
    out = tmp_path / "burned.tif"
    terrio.rasterize(layer, str(out), burn=1, ts=(15, 5), nodata=255)
    assert 0 < terrio.open_raster(str(out)).read(1).sum() < 75
    polys = terrio.polygonize(r, field="v")
    assert polys.geometry_type == "Polygon"
    assert sorted(polys.columns()["v"]) == [3, 7]
    path = tmp_path / "polys.gpkg"
    terrio.polygonize(r, str(path), connectedness=8)
    assert len(terrio.open_vector(str(path))[0]) == 2
    with pytest.raises(terrio.InvalidArgumentError):
        terrio.rasterize(layer, tr=(1, 1))


def test_info_and_drivers(tmp_path):
    _, r = make_raster()
    path = tmp_path / "i.tif"
    r.write(str(path))
    summary = terrio.info(str(path), stats=True)
    assert summary["size"] == [8, 6]
    assert summary["band_count"] == 2
    assert summary["bands"][0]["statistics"]["min"] == 0
    names = {d["name"] for d in terrio.drivers()}
    assert {"GTiff", "COG", "GPKG", "GeoJSON"} <= names


def test_transform_points():
    xs, ys = terrio.transform_points([0, 180], [0, 0], "EPSG:4326", "EPSG:3857")
    assert xs == pytest.approx([0, 20037508.342789244])
    assert ys == pytest.approx([0, 0], abs=1e-6)
    x, y = terrio.transform_points(np.array([699960.0]), [3900000], "EPSG:32610", "EPSG:4326")
    assert x[0] == pytest.approx(-120.8029, abs=1e-4)
    assert y[0] == pytest.approx(35.2231, abs=1e-4)
    with pytest.raises(terrio.InvalidArgumentError):
        terrio.transform_points([1, 2], [1], "EPSG:4326", "EPSG:3857")


def test_errors():
    with pytest.raises(terrio.NotFoundError) as e:
        terrio.open("does/not/exist.tif")
    assert isinstance(e.value, FileNotFoundError)
    assert isinstance(e.value, OSError)
    assert isinstance(e.value, terrio.TerrioError)
    with pytest.raises(terrio.CRSError) as e:
        terrio.transform_points([0], [0], "EPSG:4326", "nonsense")
    assert isinstance(e.value, ValueError)
    with pytest.raises(terrio.UnsupportedError):
        terrio.open_bytes(b"not a dataset", "x.unknown")


def test_to_geopandas():
    gpd = pytest.importorskip("geopandas")
    layer = terrio.Layer.from_geojson(POLYGONS, name="squares")
    gdf = layer.to_geopandas()
    assert isinstance(gdf, gpd.GeoDataFrame)
    assert list(gdf["name"]) == ["west", "east"]
    assert gdf.crs.to_epsg() == 4326
    assert gdf.geometry.iloc[0].area == 100
    # And back, keeping the CRS.
    back = terrio.Layer.from_geojson(gdf.to_crs(3857))
    assert back.crs == "EPSG:3857"


def test_translate():
    data, r = make_raster("int16")
    out = terrio.translate(r, srcwin=(2, 1, 4, 3), bands=[2], ot="Float32")
    assert out.dtype == "float32"
    np.testing.assert_array_equal(out.read(1), data[1, 1:4, 2:6].astype("float32"))
    assert out.transform == pytest.approx((500060.0, 30.0, 0.0, 4099970.0, 0.0, -30.0))
    half = terrio.translate(r, outsize=("50%", "50%"), resampling="average")
    assert (half.width, half.height) == (4, 3)
    scaled = terrio.translate(r, scale=(0, 95, 0, 255), ot="Byte", a_nodata="none")
    assert scaled.nodata is None
    assert scaled.read(2).max() == 255


def test_warp_and_mosaic(tmp_path):
    data, r = make_raster("float32", bands=1)
    geo = terrio.warp(r, t_srs="EPSG:4326")
    assert geo.crs == "EPSG:4326"
    west, south, east, north = geo.bounds
    assert -123.01 < west < east < -122.99 and 37.0 < south < north < 37.1
    out = tmp_path / "w.tif"
    terrio.warp([r], str(out), tr=(15, 15), resampling="bilinear")
    assert terrio.open_raster(str(out)).res == (15, 15)
    # Split in two and mosaic back.
    left = terrio.translate(r, srcwin=(0, 0, 4, 6))
    right = terrio.translate(r, srcwin=(4, 0, 4, 6))
    whole = terrio.mosaic([left, right])
    np.testing.assert_array_equal(whole.read(1), data[0])
    assert whole.transform == pytest.approx(TRANSFORM)
    paths = []
    for name, part in [("l.tif", left), ("r.tif", right)]:
        part.write(str(tmp_path / name))
        paths.append(str(tmp_path / name))
    assert terrio.mosaic(paths, str(tmp_path / "m.vrt")) is None
    np.testing.assert_array_equal(terrio.open_raster(str(tmp_path / "m.vrt")).read(1), data[0])
    with pytest.raises(terrio.InvalidArgumentError):
        terrio.mosaic([left, right], str(tmp_path / "x.vrt"))


def test_dem_products(tmp_path):
    """Checks hillshade, slope, aspect and color relief on a tilted plane."""
    cols = np.arange(8, dtype="float32")
    plane = np.tile(cols * 30.0, (6, 1))  # rises 1 m per metre eastward
    r = terrio.Raster.from_array(plane, TRANSFORM, crs="EPSG:32610", nodata=-9999)
    slope = terrio.dem(r, "slope").read(1)
    assert slope.dtype == np.float32
    assert slope[2, 3] == pytest.approx(45.0, abs=1e-4)
    assert slope[0, 0] == -9999
    aspect = terrio.dem(r, "aspect", trigonometric=True).read(1)
    assert aspect[2, 3] == pytest.approx(180.0)
    shade = terrio.dem(r, "hillshade", compute_edges=True, dst=str(tmp_path / "h.tif")).read(1)
    assert shade.dtype == np.uint8 and shade.min() > 0
    colors = tmp_path / "colors.txt"
    colors.write_text("0 0 0 255\n210 255 0 0\n")
    rgb = terrio.dem(r, "color-relief", color_file=str(colors), alpha=True)
    assert rgb.count == 4
    assert tuple(rgb.read()[:, 0, 7]) == (255, 0, 0, 255)
    with pytest.raises(terrio.InvalidArgumentError):
        terrio.dem(r, "slope", multidirectional=True)


def test_calc_numpy_typing():
    """Checks that calc follows NumPy's typing rules, as gdal_calc.py does."""
    a = np.array([[200, 3], [0, 100]], dtype="uint8")
    b = np.array([[100, 7], [5, 0]], dtype="uint8")
    ra = terrio.Raster.from_array(a, TRANSFORM, crs="EPSG:32610", nodata=0)
    rb = terrio.Raster.from_array(b, TRANSFORM, crs="EPSG:32610", nodata=0)
    out = terrio.calc("A + B", A=ra, B=rb)
    assert out.read(1).dtype == np.uint8
    assert out.nodata == 255
    np.testing.assert_array_equal(out.read(1), [[44, 10], [255, 255]])
    ndvi = terrio.calc(
        "(B.astype(float32) - A) / (B.astype(float32) + A)", A=ra, B=rb, type="Float32", nodata=-1
    )
    np.testing.assert_allclose(ndvi.read(1), [[-1 / 3, 0.4], [-1, -1]], rtol=1e-6)
    two = terrio.calc(["A", "where(B > 4, 1, 0)"], A=ra, B=rb, hide_nodata=True)
    assert two.count == 2 and two.nodata is None
    np.testing.assert_array_equal(two.read(2), [[1, 1], [1, 0]])
    with pytest.raises(terrio.InvalidArgumentError):
        terrio.calc("A + C", A=ra)


def test_proximity_and_sieve():
    """Checks distances to target pixels and the merging of small areas."""
    data = np.zeros((5, 5), dtype="uint8")
    data[2, 2] = 1
    r = terrio.Raster.from_array(data, TRANSFORM, crs="EPSG:32610")
    dist = terrio.proximity(r).read(1)
    assert dist[2, 4] == 2.0 and dist[2, 2] == 0.0
    geo = terrio.proximity(r, distunits="GEO", maxdist=45, nodata=-1).read(1)
    assert geo[2, 3] == 30.0 and geo[0, 0] == -1
    sieved = terrio.sieve(r, threshold=2).read(1)
    assert sieved[2, 2] == 0
    assert terrio.sieve(r, threshold=1).read(1)[2, 2] == 1
