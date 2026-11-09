"""terrio.xarray on synthetic rasters: opening, laziness, thread safety, accessors, writing.

Comparisons with rioxarray are in `test_xarray_vs_rioxarray.py`; these tests need xarray and dask
only.
"""

import gc
import pickle
import subprocess
import sys
import threading
import weakref

import numpy as np
import pytest

xr = pytest.importorskip("xarray")
pytest.importorskip("dask")

import dask  # noqa: E402
from affine import Affine  # noqa: E402

import terrio  # noqa: E402
import terrio.rio as rio  # noqa: E402
import terrio.xarray as txr  # noqa: E402
from terrio.rio import _tiff  # noqa: E402
from terrio.xarray import _io  # noqa: E402
from terrio.xarray.exceptions import MissingCRS  # noqa: E402

TRANSFORM = Affine(30.0, 0.0, 500000.0, 0.0, -30.0, 4100000.0)


def _surface(h=300, w=400):
    yy, xx = np.mgrid[0:h, 0:w]
    return np.sin(xx / 23.0) * np.cos(yy / 17.0) * 1000 + 1500


@pytest.fixture(scope="module")
def files(tmp_path_factory):
    """Synthetic GeoTIFFs written with terrio.rio."""
    d = tmp_path_factory.mktemp("xarray")
    out = {}
    dem = _surface().astype("int16")
    dem[200:230, 300:360] = -32768
    p = str(d / "dem.tif")
    profile = dict(
        driver="GTiff",
        width=400,
        height=300,
        count=1,
        dtype="int16",
        crs="EPSG:32611",
        transform=TRANSFORM,
        nodata=-32768,
        tiled=True,
        blockxsize=64,
        blockysize=64,
    )
    with rio.open(p, "w", **profile) as dst:
        dst.write(dem, 1)
        dst.scales = [0.5]
        dst.offsets = [100.0]
        dst.descriptions = ["elevation"]
        dst.update_tags(SENSOR="synthetic")
        dst.update_tags(1, note="half metres")
        dst.build_overviews([2, 4])
    out["dem"] = p
    rgb = np.stack([(_surface() / 12 + i * 20) % 250 + 1 for i in range(3)]).astype("uint8")
    rgb[:, 10:40, 20:80] = 0
    p = str(d / "rgb.tif")
    profile.update(count=3, dtype="uint8", nodata=0)
    with rio.open(p, "w", **profile) as dst:
        dst.write(rgb)
        dst.descriptions = ["red", "green", "blue"]
    out["rgb"] = p
    out["dir"] = d
    return out


def test_open_rasterio(files):
    da = txr.open_rasterio(files["dem"])
    assert da.dims == ("band", "y", "x")
    assert da.shape == (1, 300, 400)
    assert da.dtype == np.int16
    # Pixel centres.
    assert da.x.values[0] == 500015.0 and da.y.values[0] == 4099985.0
    assert np.allclose(np.diff(da.x.values), 30.0) and np.allclose(np.diff(da.y.values), -30.0)
    np.testing.assert_array_equal(da.band.values, [1])
    sr = da.spatial_ref.attrs
    assert sr["grid_mapping_name"] == "transverse_mercator"
    assert sr["longitude_of_central_meridian"] == -117.0
    assert sr["GeoTransform"] == "500000.0 30.0 0.0 4100000.0 0.0 -30.0"
    assert sr["crs_wkt"] == sr["spatial_ref"] and "UTM zone 11N" in sr["crs_wkt"]
    assert da.encoding["grid_mapping"] == "spatial_ref"
    assert da.encoding["preferred_chunks"] == {"y": 64, "x": 64, "band": 1}
    assert da.attrs["_FillValue"] == -32768 and isinstance(da.attrs["_FillValue"], np.int16)
    assert da.attrs["scale_factor"] == 0.5 and da.attrs["add_offset"] == 100.0
    assert da.attrs["long_name"] == "elevation"
    assert da.attrs["SENSOR"] == "synthetic" and da.attrs["note"] == "half metres"
    assert da.terrio.crs == "EPSG:32611"
    assert da.terrio.transform() == TRANSFORM
    assert da.terrio.bounds() == (500000.0, 4091000.0, 512000.0, 4100000.0)
    assert da.terrio.nodata == -32768
    raw = terrio.open_raster(files["dem"]).read()
    np.testing.assert_array_equal(da.values, raw)


def test_masked_and_scaled(files):
    raw = terrio.open_raster(files["dem"]).read(1)
    masked = txr.open_rasterio(files["dem"], masked=True)
    assert masked.dtype == np.float32
    assert masked.encoding["_FillValue"] == -32768 and masked.encoding["dtype"] == "int16"
    assert "_FillValue" not in masked.attrs
    assert np.isnan(masked.values[0, 210, 310]) and not np.isnan(masked.values[0, 0, 0])
    assert np.isnan(masked.terrio.nodata) and masked.terrio.encoded_nodata == -32768
    scaled = txr.open_rasterio(files["dem"], mask_and_scale=True)
    assert scaled.encoding["scale_factor"] == 0.5 and scaled.encoding["add_offset"] == 100.0
    assert "scale_factor" not in scaled.attrs
    np.testing.assert_allclose(scaled.values[0, 5, 5], raw[5, 5] * 0.5 + 100.0)
    rgb = txr.open_rasterio(files["rgb"], masked=True)
    assert rgb.attrs["long_name"] == ("red", "green", "blue")
    assert np.isnan(rgb.values[:, 20, 30]).all()


def test_scale_only_and_offset_only(tmp_path):
    raw = np.arange(12, dtype="int16").reshape(2, 2, 3)
    p = str(tmp_path / "so.tif")
    profile = dict(driver="GTiff", width=3, height=2, count=2, dtype="int16", crs="EPSG:32611")
    with rio.open(p, "w", transform=TRANSFORM, **profile) as dst:
        dst.write(raw)
        dst.scales = [2.0, 1.0]
        dst.offsets = [0.0, 10.0]
    with pytest.warns(UserWarning, match="differ across bands"):
        got = txr.open_rasterio(p, mask_and_scale=True).values
    np.testing.assert_array_equal(got[0], raw[0] * 2.0)
    np.testing.assert_array_equal(got[1], raw[1] + 10.0)


def test_reads_only_the_window(files, monkeypatch):
    windows = []
    read = _io._Source.read

    def spy(self, bands, window):
        windows.append((list(bands), window))
        return read(self, bands, window)

    monkeypatch.setattr(_io._Source, "read", spy)
    da = txr.open_rasterio(files["rgb"])
    assert not windows  # nothing read when opening
    sub = da.isel(band=[0, 2], x=slice(100, 150), y=slice(20, 60))
    assert not windows  # nor when slicing
    values = sub.values
    assert windows == [([1, 3], ((20, 60), (100, 150)))]
    np.testing.assert_array_equal(
        values, terrio.open_raster(files["rgb"]).read()[[0, 2], 20:60, 100:150]
    )
    windows.clear()
    da.sel(x=slice(500100, 500400), y=slice(4099000, 4098000)).load()
    assert len(windows) == 1 and windows[0][1] == ((33, 67), (3, 13))


def test_dask_threads(files):
    da = txr.open_rasterio(files["rgb"], chunks={"x": 37, "y": 23, "band": 1}, masked=True)
    ref = txr.open_rasterio(files["rgb"], masked=True).values
    for _ in range(3):
        with dask.config.set(scheduler="threads", num_workers=16):
            np.testing.assert_array_equal(da.values, ref)
    # Many threads reading one lazily opened array at once.
    lazy = txr.open_rasterio(files["rgb"], cache=False)
    raw = terrio.open_raster(files["rgb"]).read()
    errors = []

    def work(i):
        try:
            v = lazy.isel(x=slice(i, i + 50), y=slice(i, i + 40)).values
            np.testing.assert_array_equal(v, raw[:, i : i + 40, i : i + 50])
        except Exception as err:  # pragma: no cover
            errors.append(err)

    threads = [threading.Thread(target=work, args=(i,)) for i in range(0, 200, 5)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    assert not errors


def test_chunks_auto_and_lock(files):
    da = txr.open_rasterio(files["rgb"], chunks=True)
    assert da.chunks[0] == (1, 1, 1)
    single = txr.open_rasterio(files["rgb"], chunks={"x": 64, "y": 64}, lock=True)
    np.testing.assert_array_equal(single.values, da.values)


def test_pickle_and_processes(files):
    da = txr.open_rasterio(files["dem"], chunks={"x": 128, "y": 128})
    again = pickle.loads(pickle.dumps(da))
    np.testing.assert_array_equal(again.values, da.values)
    with dask.config.set(scheduler="processes", num_workers=2):
        total = float(da.astype("float64").sum().compute())
    assert total == float(terrio.open_raster(files["dem"]).read().astype("float64").sum())


def test_overview_level(files):
    full = txr.open_rasterio(files["dem"])
    ovr = txr.open_rasterio(files["dem"], overview_level=0)
    assert ovr.shape == (1, 150, 200)
    assert ovr.terrio.transform() == Affine(60.0, 0.0, 500000.0, 0.0, -60.0, 4100000.0)
    assert ovr.terrio.bounds() == full.terrio.bounds()
    # As GDAL's overview datasets: no band metadata.
    assert "long_name" not in ovr.attrs and ovr.attrs["scale_factor"] == 1.0
    raw = terrio.open_raster(files["dem"], lazy=True).read(overview=0)
    np.testing.assert_array_equal(ovr.values, raw)
    with pytest.raises(ValueError):
        txr.open_rasterio(files["dem"], overview_level=5)


def test_band_as_variable_and_engine(files):
    ds = txr.open_rasterio(files["rgb"], band_as_variable=True)
    assert list(ds.data_vars) == ["band_1", "band_2", "band_3"]
    assert ds.band_1.dims == ("y", "x") and ds.band_2.attrs["long_name"] == "green"
    engine = xr.open_dataset(files["rgb"], engine="terrio")
    assert list(engine.data_vars) == ["band_data"]
    assert engine.band_data.dtype == np.float32  # mask_and_scale is the backend's default
    assert engine.terrio.crs == "EPSG:32611"
    da = xr.open_dataarray(files["dem"], engine="terrio", chunks={})
    assert da.chunks == ((1,), (64,) * 4 + (44,), (64,) * 6 + (16,))
    assert "terrio" in xr.backends.list_engines()
    from terrio.xarray._backend import TerrioBackendEntrypoint

    assert TerrioBackendEntrypoint().guess_can_open("a/b.TIF")
    assert not TerrioBackendEntrypoint().guess_can_open("a/b.nc")


def test_clip_takes_a_layer(files):
    """A Layer clips as its GeoJSON does, from its own CRS, without contradicting `crs=`."""
    from terrio.rio.features import geometry_mask

    da = txr.open_rasterio(files["dem"], masked=True)
    polys = [
        {
            "type": "Polygon",
            "coordinates": [
                [(503000, 4093000), (506000, 4093000), (503000, 4096000), (503000, 4093000)]
            ],
        },
        {
            "type": "Polygon",
            "coordinates": [
                [(507000, 4094000), (508000, 4094000), (508000, 4095000), (507000, 4094000)]
            ],
        },
    ]
    features = [{"type": "Feature", "properties": {}, "geometry": p} for p in polys]
    layer = terrio.Layer.from_geojson(
        {"type": "FeatureCollection", "features": features}, crs=da.terrio.crs.to_wkt()
    )
    for kw in ({}, {"all_touched": True}, {"drop": False, "invert": True}):
        xr.testing.assert_identical(da.terrio.clip(layer, **kw), da.terrio.clip(polys, **kw))
    lon_lat = layer.to_crs("EPSG:4326")
    lon_lat_polys = [f["geometry"] for f in lon_lat.to_geojson()["features"]]
    xr.testing.assert_identical(
        da.terrio.clip(lon_lat), da.terrio.clip(lon_lat_polys, crs="EPSG:4326")
    )
    ds = xr.Dataset({"a": da, "b": da * 2})
    xr.testing.assert_identical(ds.terrio.clip(layer), ds.terrio.clip(polys))
    with pytest.raises(ValueError, match="Layer's own CRS"):
        da.terrio.clip(layer, crs="EPSG:4326")
    with pytest.raises(ValueError, match="must be 2D"):
        geometry_mask(layer, (2, 400, 400), da.terrio.transform())
    # A Layer without a CRS of its own takes `crs=`.
    pa = pytest.importorskip("pyarrow")
    bare = terrio.Layer.from_arrow(pa.table({"geometry": pa.array(lon_lat.to_wkb())}))
    assert bare.crs is None
    xr.testing.assert_identical(
        da.terrio.clip(bare, crs="EPSG:4326"), da.terrio.clip(lon_lat_polys, crs="EPSG:4326")
    )
    # A grid terrio does not burn on directly (south-up) goes through GeoJSON.
    south_up = Affine(30.0, 0.0, 500000.0, 0.0, 30.0, 4090000.0)
    np.testing.assert_array_equal(
        geometry_mask(layer, (400, 400), south_up), geometry_mask(polys, (400, 400), south_up)
    )


def test_clip_reproject_pad(files):
    da = txr.open_rasterio(files["dem"], masked=True)
    acc = da.terrio
    sub = acc.clip_box(503000, 4093010, 506000, 4096010)
    assert sub.terrio.bounds() == (503000.0, 4093010.0, 506000.0, 4096010.0)
    assert sub.terrio.transform() == Affine(30.0, 0.0, 503000.0, 0.0, -30.0, 4096010.0)
    np.testing.assert_array_equal(sub.values, da.values[:, 133:233, 100:200])
    lon_lat = acc.clip_box(-117.0, 37.0, -116.97, 37.03, crs="EPSG:4326")
    assert lon_lat.terrio.crs == da.terrio.crs and lon_lat.size > 0
    poly = {
        "type": "Polygon",
        "coordinates": [
            [(503000, 4093000), (506000, 4093000), (503000, 4096000), (503000, 4093000)]
        ],
    }
    clipped = acc.clip([poly])
    assert clipped.shape == (1, 99, 99)  # pixels whose centre is inside
    assert np.isnan(clipped.values[0, 0, -1]) and not np.isnan(clipped.values[0, -1, 0])
    padded = sub.terrio.pad_box(*acc.bounds())
    assert padded.shape == da.shape
    assert padded.terrio.transform() == TRANSFORM
    geo = sub.terrio.reproject("EPSG:4326", resampling="bilinear")
    assert geo.terrio.crs == "EPSG:4326"
    assert geo.spatial_ref.attrs["grid_mapping_name"] == "latitude_longitude"
    assert geo.x.attrs["standard_name"] == "longitude"
    back = geo.terrio.reproject_match(sub)
    assert back.shape == sub.shape and (back.x.values == sub.x.values).all()
    utm = da.terrio.estimate_utm_crs()
    assert utm.to_epsg() == 32611
    with pytest.raises(MissingCRS):
        xr.DataArray(np.zeros((3, 3)), dims=("y", "x")).terrio.reproject("EPSG:4326")


def test_south_up_reproject_and_write(tmp_path):
    data = np.arange(12, dtype="float32").reshape(3, 4)
    da = xr.DataArray(
        data,
        dims=("y", "x"),
        coords={"y": [10.5, 11.5, 12.5], "x": [100.5, 101.5, 102.5, 103.5]},
    ).terrio.write_crs("EPSG:32611")
    assert da.terrio.transform() == Affine(1.0, 0.0, 100.0, 0.0, 1.0, 10.0)
    same = da.terrio.reproject("EPSG:32611", transform=da.terrio.transform(), shape=(3, 4))
    np.testing.assert_array_equal(same.values, data)
    p = str(tmp_path / "south_up.tif")
    da.terrio.to_raster(p)
    back = txr.open_rasterio(p)
    np.testing.assert_array_equal(back.values[0], data[::-1])
    assert back.terrio.transform() == Affine(1.0, 0.0, 100.0, 0.0, -1.0, 13.0)


def test_south_up_reproject_keeps_the_source_s_pixel_edges():
    """A south-up array is warped through its own transform, as rioxarray hands it to GDAL.

    A destination pixel centre on the edge between two source rows falls in the row that edge
    starts in the source's order (the northern row, latitudes ascending), and a centre within
    1e-10 pixel of the source's far edge (the north pole) is outside it. Flipped north-up
    first, the source gave the southern row, and values at the north pole but not the south.
    """
    # Latitudes 19 to 20, 1/9 degree apart: destination row 3's centre is 19.5, the edge
    # between the source rows at 19.444 (4) and 19.556 (5).
    v = np.random.default_rng(42).random((10, 10), dtype=np.float32)
    da = xr.DataArray(
        v,
        dims=("y", "x"),
        coords={"y": np.linspace(19.0, 20.0, 10), "x": np.linspace(-156.0, -155.0, 10)},
    ).terrio.write_crs("EPSG:4326")
    t = rio.transform.from_bounds(-155.9, 19.1, -155.1, 19.9, 7, 7)
    out = da.terrio.reproject("EPSG:4326", shape=(7, 7), transform=t)
    rows, cols = [8, 7, 6, 5, 3, 2, 1], [1, 2, 3, 5, 6, 7, 8]
    np.testing.assert_array_equal(out.values, v[np.ix_(rows, cols)])
    # A global 1-degree grid onto Web Mercator's default grid (401 x 33): its 54 top rows have
    # centres within 1e-10 degree of the north pole and stay empty; every other pixel is warped.
    g = xr.DataArray(
        np.ones((180, 360), "float32"),
        dims=("lat", "lon"),
        coords={"lat": np.arange(-89.5, 90.0), "lon": np.arange(-179.5, 180.0)},
    )
    g = g.terrio.set_spatial_dims(x_dim="lon", y_dim="lat").terrio.write_crs("EPSG:4326")
    m = g.terrio.reproject("EPSG:3857", nodata=np.nan).values
    assert m.shape == (401, 33)
    np.testing.assert_array_equal(np.isnan(m).all(axis=1), np.arange(401) < 54)
    assert int(np.isnan(m).sum()) == 54 * 33 and (m[54:] == 1.0).all()


def test_to_raster_round_trips(files, tmp_path):
    da = txr.open_rasterio(files["dem"], mask_and_scale=True, chunks={"x": 100, "y": 100})
    for name, kw in [
        ("plain.tif", {}),
        ("cog.tif", {"driver": "COG", "compress": "DEFLATE"}),
        ("windowed.tif", {"windowed": True}),
    ]:
        p = str(tmp_path / name)
        da.terrio.to_raster(p, **kw)
        back = txr.open_rasterio(p, mask_and_scale=True)
        np.testing.assert_array_equal(back.values, da.values)
        assert back.terrio.crs == da.terrio.crs
        assert back.terrio.transform() == da.terrio.transform()
        assert back.encoding["scale_factor"] == 0.5 and back.encoding["add_offset"] == 100.0
        assert back.encoding["_FillValue"] == -32768
    with rio.open(str(tmp_path / "cog.tif")) as src:
        assert src.profile["compress"] == "deflate" and src.profile["tiled"]
    # terrio's Zarr writer stores no scales and offsets: it says so.
    with pytest.warns(UserWarning, match="scales and offsets"):
        da.terrio.to_raster(str(tmp_path / "scaled.zarr"))
    plain = txr.open_rasterio(files["rgb"], chunks={"x": 100, "y": 100})
    plain.terrio.to_raster(str(tmp_path / "out.zarr"))
    back = txr.open_rasterio(str(tmp_path / "out.zarr"))
    np.testing.assert_array_equal(back.values, plain.values)
    assert back.terrio.crs == plain.terrio.crs
    assert back.terrio.transform() == plain.terrio.transform()
    delayed = da.terrio.to_raster(str(tmp_path / "later.tif"), compute=False)
    assert not (tmp_path / "later.tif").exists()
    delayed.compute()
    later = txr.open_rasterio(str(tmp_path / "later.tif"), mask_and_scale=True)
    np.testing.assert_array_equal(later.values, da.values)
    ds = txr.open_rasterio(files["rgb"], band_as_variable=True)
    ds.terrio.to_raster(str(tmp_path / "ds.tif"))
    with rio.open(str(tmp_path / "ds.tif")) as src:
        assert src.count == 3 and src.descriptions == ("red", "green", "blue")


@pytest.mark.parametrize("source_nodata", [None, -32768])
@pytest.mark.parametrize("mode", ["eager", "windowed", "delayed"])
def test_masked_integer_warp_export(tmp_path, source_nodata, mode):
    """Warp borders round-trip with their grid and floating or encoded integer nodata.

    The GDAL 3.13.3 cross-read is a separate manual validation.
    """
    source = tmp_path / "source.tif"
    pixels = np.arange(16, dtype="int16").reshape(4, 4)
    with rio.open(
        source,
        "w",
        driver="GTiff",
        width=4,
        height=4,
        count=1,
        dtype="int16",
        crs="EPSG:32611",
        transform=TRANSFORM,
        nodata=source_nodata,
    ) as dst:
        dst.write(pixels, 1)
    with txr.open_rasterio(source, masked=True) as original:
        da = original.terrio.reproject(
            original.terrio.crs,
            shape=(6, 6),
            transform=TRANSFORM @ Affine.translation(-1, -1),
        )
        encoding = da.encoding.copy()
        assert np.isnan(da.values[0, 0, 0])
        np.testing.assert_array_equal(da.values[0, 1:5, 1:5], pixels)
        if mode == "delayed":
            da = da.chunk({"x": 2, "y": 2})
        target = tmp_path / "out.tif"
        result = da.terrio.to_raster(
            target,
            windowed=mode == "windowed",
            compute=mode != "delayed",
        )
        if mode == "delayed":
            assert not target.exists()
            result.compute()
        assert da.encoding == encoding
        with txr.open_rasterio(target, masked=True) as back:
            np.testing.assert_array_equal(back.values, da.values)
            assert back.terrio.crs == da.terrio.crs
            assert back.terrio.transform() == da.terrio.transform()
        with rio.open(target) as raw:
            assert raw.dtypes == (("float32",) if source_nodata is None else ("int16",))
            if source_nodata is None:
                assert np.isnan(raw.nodata)
            else:
                assert raw.nodata == source_nodata
        if source_nodata is None:
            with pytest.raises(ValueError, match="write_nodata.*encoded=True"):
                da.terrio.to_raster(tmp_path / "invalid.tif", dtype="int16")
            assert not (tmp_path / "invalid.tif").exists()
            encoded = da.terrio.write_nodata(-32768, encoded=True)
            encoded.terrio.to_raster(tmp_path / "integer.tif", dtype="int16")
            with txr.open_rasterio(tmp_path / "integer.tif", masked=True) as back:
                np.testing.assert_array_equal(back.values, da.values)


def test_write_nan_nodata_to_integer_keeps_value_error():
    da = xr.DataArray(np.ones((2, 2), dtype="int16"), dims=("y", "x"))
    with pytest.raises(ValueError, match="nodata value.*nan.*dtype.*int16"):
        da.terrio.write_nodata(np.nan)


def test_to_raster_float_override_preserves_fractions(tmp_path):
    da = (
        xr.DataArray(
            np.array([[1.25, 2.75], [3.5, np.nan]], dtype="float32"),
            dims=("y", "x"),
        )
        .terrio.write_crs("EPSG:32611")
        .terrio.write_transform(TRANSFORM)
    )
    da.encoding["dtype"] = "int16"
    da.terrio.write_nodata(np.nan, inplace=True)
    target = tmp_path / "float.tif"
    da.terrio.to_raster(target, dtype="float32")
    with txr.open_rasterio(target) as back:
        np.testing.assert_array_equal(back.values[0], da.values)
    assert da.encoding["dtype"] == "int16"


def test_float_override_keeps_packed_values_and_metadata(files, tmp_path):
    """GDAL 3.13.3 cross-read: dtype overrides retain CF scales and offsets."""
    gdal = pytest.importorskip("osgeo.gdal")
    da = txr.open_rasterio(files["dem"], mask_and_scale=True)
    target = tmp_path / "packed-float.tif"
    da.terrio.to_raster(target, dtype="float32")
    with gdal.ExceptionMgr(), gdal.Open(str(target)) as dataset:
        band = dataset.GetRasterBand(1)
        assert band.DataType == gdal.GDT_Float32
        assert band.GetScale() == 0.5
        assert band.GetOffset() == 100.0
        assert band.GetNoDataValue() == -32768
        np.testing.assert_array_equal(
            dataset.ReadAsArray(), terrio.open_raster(files["dem"]).read(1)
        )
    with txr.open_rasterio(target, mask_and_scale=True) as back:
        np.testing.assert_array_equal(back.values, da.values)


def test_to_zarr_georeferencing(files, tmp_path):
    pytest.importorskip("zarr")
    da = txr.open_rasterio(files["rgb"], masked=True)
    store = str(tmp_path / "x.zarr")
    da.to_dataset(name="band_data").to_zarr(store, mode="w", consolidated=False)
    back = xr.open_zarr(store, consolidated=False, decode_coords="all").band_data
    assert back.terrio.crs == da.terrio.crs
    assert back.terrio.transform() == da.terrio.transform()
    np.testing.assert_array_equal(back.values, da.values)
    core = terrio.open_raster(store)
    assert core.crs == "EPSG:32611" and core.transform == TRANSFORM.to_gdal()


def test_merge(files):
    da = txr.open_rasterio(files["dem"])
    a = da.terrio.clip_box(500000, 4091000, 508000, 4100000)
    b = da.terrio.clip_box(504000, 4091000, 512000, 4100000)
    merged = txr.merge_arrays([a, b])
    np.testing.assert_array_equal(merged.values, da.values)
    assert merged.terrio.transform() == TRANSFORM
    mds = txr.merge_datasets([a.to_dataset(name="v"), b.to_dataset(name="v")])
    np.testing.assert_array_equal(mds.v.values, da.values)


def test_crs_to_cf():
    from terrio.rio.crs import CRS

    cf = txr.crs_to_cf(CRS.from_epsg(32610))
    assert cf["grid_mapping_name"] == "transverse_mercator"
    assert cf["scale_factor_at_central_meridian"] == 0.9996
    assert cf["horizontal_datum_name"] == "World Geodetic System 1984"
    assert cf["semi_minor_axis"] == 6356752.314245179
    cf = txr.crs_to_cf(CRS.from_epsg(5070))
    assert cf["grid_mapping_name"] == "albers_conical_equal_area"
    assert cf["standard_parallel"] == (29.5, 45.5)
    cf = txr.crs_to_cf(CRS.from_epsg(3413))
    assert cf["grid_mapping_name"] == "polar_stereographic" and cf["standard_parallel"] == 70.0
    assert txr.crs_to_cf(CRS.from_epsg(4269))["grid_mapping_name"] == "latitude_longitude"
    assert txr.crs_to_cf(CRS.from_epsg(3857)) == {}  # no CF grid mapping, as in pyproj
    pyproj = pytest.importorskip("pyproj")
    for code in (4326, 4269, 32610, 32733, 5070, 3413, 3031, 2056, 27700, 3035, 2154):
        ours = txr.crs_to_cf(CRS.from_epsg(code))
        theirs = pyproj.CRS.from_wkt(CRS.from_epsg(code).to_wkt()).to_cf()
        theirs.pop("crs_wkt")
        assert set(ours) == set(theirs), code
        for k, v in theirs.items():
            if isinstance(v, float):
                assert ours[k] == pytest.approx(v, rel=1e-12), (code, k)
            else:
                assert ours[k] == v, (code, k)


def test_register_rio_accessor(files):
    code = f"""
import sys
import terrio.xarray as txr
assert "rioxarray" not in sys.modules
assert txr.register_rio_accessor()
da = txr.open_rasterio({files["dem"]!r}).rename(x="e", y="n")
da.rio.set_spatial_dims(x_dim="e", y_dim="n", inplace=True)
assert da.terrio.x_dim == "e"  # one accessor, two names
print(da.rio.crs.to_epsg(), da.rio.bounds())
"""
    out = subprocess.run([sys.executable, "-c", code], capture_output=True, text=True, check=True)
    assert out.stdout.startswith("32611 (500000.0, 4091000.0, 512000.0, 4100000.0)")


def test_unsupported(files):
    with pytest.raises(NotImplementedError):
        txr.open_rasterio(files["dem"]).terrio.write_crs("EPSG:4326", convention="ZARR")


def test_subdatasets_gcps_and_rpcs(tmp_path):
    """Subdatasets as a Dataset (filtered by group), and GCPs and RPCs written and read back."""
    import terrio
    from terrio.rio.control import GroundControlPoint

    t = (480000.0, 30.0, 0.0, 4400000.0, 0.0, -30.0)
    a = terrio.Raster.from_array(np.arange(12, dtype="float32").reshape(3, 4), t, "EPSG:32613")
    b = terrio.Raster.from_array(np.ones((2, 5), dtype="int16"), t, "EPSG:32613")
    nc = str(tmp_path / "groups.nc")
    terrio.write_netcdf_groups({"/g/a": a, "/g/b": b, "/c": a}, nc)
    out = txr.open_rasterio(nc)
    assert isinstance(out, list) and len(out) == 2  # two shapes
    # Groups by their full path, as rioxarray matches them.
    assert list(txr.open_rasterio(nc, group="g/a", variable="Band1").data_vars) == ["Band1"]
    assert len(txr.open_rasterio(nc, group="g").data_vars) == 0
    one = txr.open_rasterio(nc, group="c")
    np.testing.assert_array_equal(one["Band1"].values[0], np.arange(12).reshape(3, 4))

    gcps = [
        GroundControlPoint(row=r, col=c, x=-105 + c * 0.01, y=40 - r * 0.01)
        for r in (0, 2)
        for c in (0, 3)
    ]
    da = xr.DataArray(np.zeros((1, 3, 4), "uint8"), dims=("band", "y", "x"))
    # Without a CRS too, a copy: the caller's array is left as it was.
    da.terrio.write_gcps(gcps, None)
    assert "spatial_ref" not in da.coords and da.terrio.get_gcps() is None
    da = da.terrio.write_gcps(gcps, "EPSG:4326")
    assert [(g.row, g.col, g.x, g.y) for g in da.terrio.get_gcps()] == [
        (g.row, g.col, g.x, g.y) for g in gcps
    ]
    tif = str(tmp_path / "gcps.tif")
    da.terrio.to_raster(tif)
    back = txr.open_rasterio(tif)
    assert len(back.terrio.get_gcps()) == 4 and "x" not in back.coords
    assert str(back.terrio.crs) == "EPSG:4326"


def test_utm_zones():
    from terrio.xarray._accessor import _utm_epsg

    assert _utm_epsg(180.0, 10.0, "WGS 84") == 32660
    assert _utm_epsg(-180.0, 10.0, "WGS 84") == 32601
    assert _utm_epsg(-102.0, 40.0, "WGS 84") == 32613  # a boundary: the lower zone, as PROJ
    assert _utm_epsg(-101.9, -40.0, "WGS 84") == 32714
    assert _utm_epsg(-105.0, 40.0, "NAD83") == 26913


def test_deep_copy_with_a_lock(files):
    lock = threading.Lock()
    da = txr.open_rasterio(files["dem"], lock=lock)
    copied = da.terrio.write_nodata(0)  # a deep copy of the lazy array
    wrapper = copied.variable._data.array.array.array
    assert wrapper.lock is lock
    np.testing.assert_array_equal(copied.values, da.values)


def test_merge_scalar_resolution(files, monkeypatch):
    da = txr.open_rasterio(files["dem"])
    a = da.terrio.clip_box(500000, 4091000, 508000, 4100000)
    b = da.terrio.clip_box(504000, 4091000, 512000, 4100000)
    calls = []
    reproject = txr.TerrioArray.reproject

    def spy(self, *args, **kwargs):
        calls.append(args)
        return reproject(self, *args, **kwargs)

    monkeypatch.setattr(txr.TerrioArray, "reproject", spy)
    merged = txr.merge_arrays([a, b], res=30)
    assert not calls  # arrays already at 30 m are not reprojected
    np.testing.assert_array_equal(merged.values, da.values)
    assert txr.merge_arrays([a, b], res=[30]).shape == da.shape


def test_to_raster_lazy_and_options(files, tmp_path, monkeypatch):
    from terrio.xarray import _writer

    chunks = []
    setitem = _writer._StoreTarget.__setitem__

    def spy_setitem(self, key, item):
        chunks.append(key)
        return setitem(self, key, item)

    monkeypatch.setattr(_writer._StoreTarget, "__setitem__", spy_setitem)
    da = txr.open_rasterio(files["rgb"], chunks={"x": 100, "y": 100})
    delayed = da.terrio.to_raster(str(tmp_path / "later.tif"), compute=False)
    delayed.compute()
    assert len(chunks) == 3 * 4  # written chunk by chunk, not computed first
    np.testing.assert_array_equal(txr.open_rasterio(str(tmp_path / "later.tif")).values, da.values)
    opened = []
    rio_open = _writer.rio_open

    def spy_open(path, mode="r", **kwargs):
        opened.append(kwargs)
        return rio_open(path, mode, **kwargs)

    monkeypatch.setattr(_writer, "rio_open", spy_open)
    txr.open_rasterio(files["rgb"]).terrio.to_raster(str(tmp_path / "x.zarr"))
    assert opened[-1]["driver"] == "Zarr"
    assert "blockxsize" not in opened[-1] and "tiled" not in opened[-1]


def test_per_dataset_mask_and_backend_close(tmp_path):
    masked_path = tmp_path / "masked.tif"
    pixels = np.array([[10, 20], [30, 40]], dtype="uint8")
    mask = np.array([[255, 0], [0, 255]], dtype="uint8")
    with rio.open(
        masked_path,
        "w",
        driver="GTiff",
        width=2,
        height=2,
        count=1,
        dtype="uint8",
        crs="EPSG:4326",
        transform=Affine(1.0, 0.0, 0.0, 0.0, -1.0, 2.0),
    ) as dst:
        dst.write(pixels, 1)
        dst.write_mask(mask)
        dst.build_overviews([2])

    da = txr.open_rasterio(str(masked_path), masked=True, cache=False)
    expected = np.array([[[10.0, np.nan], [np.nan, 40.0]]], dtype="float32")
    np.testing.assert_allclose(da[:, :1, :].values, expected[:, :1, :], equal_nan=True)
    np.testing.assert_allclose(da.values, expected, equal_nan=True)

    da_ovr = txr.open_rasterio(str(masked_path), masked=True, overview_level=0)
    assert da_ovr.shape == (1, 1, 1)
    assert da_ovr.values[0, 0, 0] == 10.0
    da_ovr.close()
    masked_ovr_path = tmp_path / "masked_ovr_4x4.tif"
    with rio.open(
        masked_ovr_path,
        "w",
        driver="GTiff",
        width=4,
        height=4,
        count=1,
        dtype="uint8",
        crs="EPSG:4326",
        transform=Affine(1.0, 0.0, 0.0, 0.0, -1.0, 4.0),
    ) as dst:
        dst.write(np.arange(1, 17, dtype="uint8").reshape(4, 4), 1)
        dst.write_mask(
            np.array(
                [[255, 255, 0, 0], [255, 255, 0, 0], [0, 0, 255, 255], [0, 0, 255, 255]],
                dtype="uint8",
            )
        )
        dst.build_overviews([2])
    da_ovr4 = txr.open_rasterio(str(masked_ovr_path), masked=True, overview_level=0)
    assert da_ovr4.shape == (1, 2, 2)
    assert np.isfinite(da_ovr4.values[0, 0, 0]) and np.isnan(da_ovr4.values[0, 0, 1])
    assert np.isnan(da_ovr4.values[0, 1, 0]) and np.isfinite(da_ovr4.values[0, 1, 1])
    da_ovr4.close()

    from terrio.xarray._backend import TerrioBackendEntrypoint

    def unwrap_pool(data_array):
        arr = data_array.variable._data
        while hasattr(arr, "array"):
            arr = arr.array
        return arr.source.pool

    ep = TerrioBackendEntrypoint()
    ds1 = ep.open_dataset(str(masked_path))
    assert ds1._close is not None
    pool1 = unwrap_pool(ds1["band_data"])
    assert pool1._free or pool1._shared is not None
    ds1.close()
    assert pool1._shared is None and not pool1._free

    rgb_path = tmp_path / "rgb_drop.tif"
    with rio.open(
        rgb_path,
        "w",
        driver="GTiff",
        width=2,
        height=2,
        count=2,
        dtype="uint8",
        crs="EPSG:4326",
        transform=Affine(1.0, 0.0, 0.0, 0.0, -1.0, 2.0),
    ) as dst:
        dst.write(pixels, 1)
        dst.write(pixels, 2)
    ds2 = ep.open_dataset(str(rgb_path), band_as_variable=True, drop_variables=("band_1",))
    assert "band_1" not in ds2.data_vars and "band_2" in ds2.data_vars
    assert ds2._close is not None
    pool2 = unwrap_pool(ds2["band_2"])
    assert pool2._free or pool2._shared is not None
    ds2.close()
    assert pool2._shared is None and not pool2._free

    ds_no_nodata = txr.open_rasterio(str(rgb_path), band_as_variable=True)
    assert ds_no_nodata["band_1"].terrio.nodata is None
    out_no_nodata = tmp_path / "rgb_no_nodata_out.tif"
    ds_no_nodata.terrio.to_raster(str(out_no_nodata))
    with rio.open(out_no_nodata) as check_ds:
        assert check_ds.nodata is None and check_ds.count == 2
    ds_no_nodata.close()
    ds_nan_nodata = txr.open_rasterio(str(rgb_path), band_as_variable=True).astype("float32")
    ds_nan_nodata["band_1"].terrio.write_nodata(np.float32("nan"), inplace=True)
    ds_nan_nodata["band_2"].terrio.write_nodata(float("nan"), inplace=True)
    out_nan_nodata = tmp_path / "rgb_nan_nodata_out.tif"
    ds_nan_nodata.terrio.to_raster(str(out_nan_nodata))
    with rio.open(out_nan_nodata) as check_nan_ds:
        assert check_nan_ds.nodata is not None and np.isnan(check_nan_ds.nodata)
        assert check_nan_ds.count == 2
    ds_nan_nodata.close()


def test_dataset_export_requires_matching_nodata(tmp_path):
    """None and NaN are distinct declarations; encoded errors report the compared values."""
    from terrio.xarray.exceptions import RioXarrayError

    base = xr.DataArray(
        np.array([[np.nan, 1], [2, 3]], dtype="float32"),
        dims=("y", "x"),
        coords={"x": [0.5, 1.5], "y": [1.5, 0.5]},
    ).terrio.write_crs("EPSG:4326")
    second = base.terrio.write_nodata(np.nan)
    dataset = xr.Dataset({"first": base, "second": second})
    output = str(tmp_path / "mismatched.tif")
    with pytest.raises(RioXarrayError, match="All nodata values must be the same"):
        dataset.terrio.to_raster(output)
    for name, value in [("first", -1), ("second", -2)]:
        dataset[name].terrio.write_nodata(value, encoded=True, inplace=True)
    with pytest.raises(RioXarrayError, match=r"Current values:.*-1.*-2"):
        dataset.terrio.to_raster(output)


@pytest.mark.parametrize(
    "mask_before_overviews,external", [(False, False), (True, False), (True, True)]
)
@pytest.mark.parametrize("has_nodata", [False, True])
def test_overview_masks_match_opened_gdal_dataset(
    tmp_path, mask_before_overviews, external, has_nodata, monkeypatch
):
    """Match GDAL 3.13.3 overview masks, including absent masks and valid nodata pixels."""
    gdal = pytest.importorskip("osgeo.gdal")
    path = str(tmp_path / "overview_mask.tif")
    pixels = np.full((8, 8), 10, dtype="float32")
    pixels[4:, 4:] = -9999
    ds = gdal.GetDriverByName("GTiff").Create(path, 8, 8, 2, gdal.GDT_Float32)
    ds.SetGeoTransform([0, 1, 0, 8, 0, -1])
    for band in (1, 2):
        ds.GetRasterBand(band).WriteArray(pixels)
        if has_nodata:
            ds.GetRasterBand(band).SetNoDataValue(-9999)
    if not mask_before_overviews:
        ds.BuildOverviews("NEAREST", [2])
    # Force an internal mask independently of the process's GDAL configuration.
    with gdal.config_option("GDAL_TIFF_INTERNAL_MASK", "YES"):
        ds.CreateMaskBand(gdal.GMF_PER_DATASET)
    mask = np.full((8, 8), 255, dtype="uint8")
    mask[:4, :4] = 0
    ds.GetRasterBand(1).GetMaskBand().WriteArray(mask)
    if mask_before_overviews and not external:
        ds.BuildOverviews("NEAREST", [2])
    ds = None
    if external:
        ds = gdal.Open(path)
        ds.BuildOverviews("NEAREST", [2])
        ds = None
    if external and has_nodata:
        # An external overview is a separate dataset: its nodata can differ from the base.
        ds = gdal.Open(path + ".ovr", gdal.GA_Update)
        for band in (1, 2):
            data = ds.GetRasterBand(band).ReadAsArray()
            data[data == -9999] = -8888
            ds.GetRasterBand(band).WriteArray(data)
            ds.GetRasterBand(band).SetNoDataValue(-8888)
        ds = None
    reference = gdal.OpenEx(path, open_options=["OVERVIEW_LEVEL=0"])
    expected = []
    for band in (1, 2):
        overview = reference.GetRasterBand(band)
        data = overview.ReadAsArray().astype("float32")
        data[overview.GetMaskBand().ReadAsArray() == 0] = np.nan
        expected.append(data)
    mask_reads = []
    read_mask = terrio.Raster._read_level_mask

    def counted_mask_read(self, *args, **kwargs):
        mask_reads.append(kwargs)
        return read_mask(self, *args, **kwargs)

    monkeypatch.setattr(terrio.Raster, "_read_level_mask", counted_mask_read)
    with txr.open_rasterio(path, masked=True, overview_level=0, cache=False) as opened:
        assert mask_reads == [], "opening overview metadata must not decode mask payload"
        np.testing.assert_allclose(
            opened[:, :2, :].values, np.stack(expected)[:, :2, :], equal_nan=True
        )
        np.testing.assert_allclose(opened.values, np.stack(expected), equal_nan=True)


@pytest.mark.parametrize("fallback", ["all_valid", "nodata", "alpha"])
def test_reduced_mask_at_full_size_uses_validity_fallback(tmp_path, fallback):
    """A reduced-resolution mask IFD is not a full-resolution dataset mask."""
    import struct

    from terrio.rio.enums import ColorInterp

    path = tmp_path / "reduced-mask.tif"
    values = np.array([[10, 20], [30, 40]], dtype="uint8")
    count = 4 if fallback == "alpha" else 1
    with rio.open(
        path,
        "w",
        driver="GTiff",
        width=2,
        height=2,
        count=count,
        dtype="uint8",
        nodata=20 if fallback == "nodata" else None,
        transform=TRANSFORM,
    ) as dst:
        for band in range(1, count + 1):
            dst.write(values if band != 4 else np.array([[255, 0], [0, 255]], dtype="uint8"), band)
        if fallback == "alpha":
            dst.colorinterp = (
                ColorInterp.red,
                ColorInterp.green,
                ColorInterp.blue,
                ColorInterp.alpha,
            )
        dst.write_mask(np.array([[0, 255], [255, 0]], dtype="uint8"))
    # Set only the mask IFD's reduced-resolution bit, keeping its dimensions unchanged.
    raw = bytearray(path.read_bytes())
    assert raw[:4] == b"II*\x00"
    offset = struct.unpack_from("<I", raw, 4)[0]
    changed = False
    while offset:
        size = struct.unpack_from("<H", raw, offset)[0]
        for index in range(size):
            entry = offset + 2 + 12 * index
            tag, kind, length, value = struct.unpack_from("<HHII", raw, entry)
            if tag == 254 and kind == 4 and length == 1 and value & 4:
                struct.pack_into("<I", raw, entry + 8, value | 1)
                changed = True
        offset = struct.unpack_from("<I", raw, offset + 2 + 12 * size)[0]
    assert changed
    path.write_bytes(raw)
    expected = values.astype("float32")
    if fallback == "nodata":
        expected[values == 20] = np.nan
    elif fallback == "alpha":
        expected[0, 1] = expected[1, 0] = np.nan
    da = txr.open_rasterio(str(path), masked=True, cache=False)
    try:
        np.testing.assert_allclose(da[:1, :1, :].values[0], expected[:1], equal_nan=True)
        np.testing.assert_allclose(da.values[0], expected, equal_nan=True)
    finally:
        da.close()


def test_rio_opened_overview_uses_overview_only_mask(tmp_path):
    """Opened overview flags and pixels use the same native mask discovery."""
    import struct

    from terrio.rio.enums import MaskFlags, Resampling

    path = tmp_path / "overview-only-mask.tif"
    values = np.arange(1, 17, dtype="uint8").reshape(4, 4)
    mask = np.full((4, 4), 255, dtype="uint8")
    mask[:, :2] = 0
    with rio.open(
        path, "w", driver="GTiff", width=4, height=4, count=1, dtype="uint8", transform=TRANSFORM
    ) as dst:
        dst.write(values, 1)
        dst.write_mask(mask)
        dst.build_overviews([2], Resampling.nearest)
    # Unlink the full-resolution mask IFD, preserving the overview's type-5 mask.
    raw = bytearray(path.read_bytes())
    assert raw[:4] == b"II*\x00"
    link = 4
    removed = False
    while offset := struct.unpack_from("<I", raw, link)[0]:
        size = struct.unpack_from("<H", raw, offset)[0]
        tags = {
            struct.unpack_from("<H", raw, offset + 2 + 12 * i)[0]: struct.unpack_from(
                "<I", raw, offset + 10 + 12 * i
            )[0]
            for i in range(size)
        }
        next_link = offset + 2 + 12 * size
        if tags.get(254, 0) == 4:
            struct.pack_into("<I", raw, link, struct.unpack_from("<I", raw, next_link)[0])
            removed = True
        else:
            link = next_link
    assert removed
    path.write_bytes(raw)
    with rio.open(path) as src:
        assert src.mask_flag_enums == ([MaskFlags.all_valid],)
    with rio.open(path, overview_level=0) as src:
        assert src.mask_flag_enums == ([MaskFlags.per_dataset],)
        actual = src.read(1, masked=True)
        np.testing.assert_array_equal(actual.mask, [[True, False], [True, False]])
        np.testing.assert_array_equal(actual[:, 1], [3, 11])


def test_sidecar_only_mask_flags_and_uncached_reads_agree(tmp_path):
    """GDAL 3.13.3 sidecar masks use the same native discovery as full-resolution reads."""
    gdal = pytest.importorskip("osgeo.gdal")
    path = tmp_path / "sidecar-mask.tif"
    pixels = np.array([[10, 20], [30, 40]], dtype="uint8")
    with gdal.config_option("GDAL_TIFF_INTERNAL_MASK", "NO"):
        dst = gdal.GetDriverByName("GTiff").Create(str(path), 2, 2, 1, gdal.GDT_Byte)
        dst.GetRasterBand(1).WriteArray(pixels)
        dst.CreateMaskBand(gdal.GMF_PER_DATASET)
        dst.GetRasterBand(1).GetMaskBand().WriteArray(np.array([[255, 0], [0, 255]], dtype="uint8"))
        dst = None
    assert path.with_suffix(".tif.msk").is_file()
    with rio.open(path) as src:
        from terrio.rio.enums import MaskFlags

        assert src.mask_flag_enums == ([MaskFlags.per_dataset],)
    expected = np.array([[[10, np.nan], [np.nan, 40]]], dtype="float32")
    with txr.open_rasterio(str(path), masked=True, cache=False) as opened:
        np.testing.assert_allclose(opened[:, :1, :].values, expected[:, :1, :], equal_nan=True)
        np.testing.assert_allclose(opened.values, expected, equal_nan=True)


def test_to_raster_keeps_pixel_is_point(tmp_path):
    """An array read from a PixelIsPoint GeoTIFF (`AREA_OR_POINT=Point` among its attributes) is
    written back PixelIsPoint, its tie point the centre of pixel (0, 0): it came out PixelIsArea."""
    source = tmp_path / "point.tif"
    pixels = np.arange(12, dtype="int16").reshape(1, 3, 4)
    terrio.Raster.from_array(
        pixels, TRANSFORM.to_gdal(), crs="EPSG:32611", metadata={"AREA_OR_POINT": "Point"}
    ).write(str(source))
    da = txr.open_rasterio(str(source))
    assert da.attrs["AREA_OR_POINT"] == "Point"
    out = tmp_path / "out.tif"
    da.terrio.to_raster(str(out))
    with rio.open(out) as src:
        assert src.tags()["AREA_OR_POINT"] == "Point" and src.transform == TRANSFORM
        np.testing.assert_array_equal(src.read(), pixels)
    tags = terrio._terrio.tiff_tags(str(out))[0]
    assert _tiff._geokey(tags, 1025) == 2
    assert tuple(tags[33922][3:5]) == (500015.0, 4099985.0)


@pytest.mark.parametrize("kind", ["DataArray", "Dataset"])
def test_accessor_leaves_no_reference_cycle(kind):
    """What `.terrio` touched is freed when dropped, without the cyclic garbage collector:
    `write_crs` reads views of each variable, which share the Dataset's variables, and
    `write_nodata` copies the object with the accessor's state."""
    coords = {"y": [3.5, 2.5, 1.5, 0.5], "x": [0.5, 1.5, 2.5, 3.5]}
    da = xr.DataArray(np.zeros((4, 4)), dims=("y", "x"), coords=coords)
    obj = da if kind == "DataArray" else xr.Dataset({"a": da, "b": da + 1})
    gc.disable()
    try:
        out = obj.terrio.write_crs("EPSG:4326").terrio.write_transform()
        if kind == "DataArray":
            out = out.terrio.write_nodata(-1)
        assert out.terrio.crs == "EPSG:4326" and out.terrio.transform() is not None
        # the arrays from the Variables: an `out.a` view would be one more object to free
        variables = out.variables if kind == "Dataset" else {"data": out.variable}
        refs = {"out": weakref.ref(out)}
        refs.update({n: weakref.ref(v.data) for n, v in variables.items() if v.ndim})
        del out, variables
        assert [n for n, r in refs.items() if r() is not None] == []
    finally:
        gc.enable()
