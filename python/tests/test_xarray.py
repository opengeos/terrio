"""terrio.xarray on synthetic rasters: opening, laziness, thread safety, accessors, writing.

Comparisons with rioxarray are in `test_xarray_vs_rioxarray.py`; these tests need xarray and dask
only.
"""

import pickle
import subprocess
import sys
import threading

import numpy as np
import pytest

xr = pytest.importorskip("xarray")
pytest.importorskip("dask")

import dask  # noqa: E402
from affine import Affine  # noqa: E402

import terrio  # noqa: E402
import terrio.rio as rio  # noqa: E402
import terrio.xarray as txr  # noqa: E402
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
