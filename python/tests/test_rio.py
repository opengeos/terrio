"""Tests of terrio.rio, the rasterio-compatible API, on synthetic data (rasterio not needed).

`test_rio_vs_rasterio.py` compares the same API with rasterio when it is installed.
"""

import io
import json
import math
import warnings

import numpy as np
import pytest
from affine import Affine

import terrio.rio as rio
from terrio.rio import features, merge, warp
from terrio.rio.enums import ColorInterp, MaskFlags, Resampling
from terrio.rio.errors import (
    CRSError,
    RasterBlockError,
    RasterioIOError,
    UnsupportedOperation,
    WindowError,
)
from terrio.rio.windows import Window

T = rio.transform.from_origin(500000, 4100000, 30, 30)


@pytest.fixture
def dem(tmp_path):
    """A tiled int16 GeoTIFF with nodata, tags, a description and overviews."""
    path = str(tmp_path / "dem.tif")
    yy, xx = np.mgrid[0:203, 0:301]
    data = ((xx * 3 + yy * 7) % 1000).astype("int16")[np.newaxis].repeat(2, axis=0)
    data[1] += 1
    data[:, 10:20, 30:60] = -9999
    profile = dict(
        driver="GTiff",
        width=301,
        height=203,
        count=2,
        dtype="int16",
        crs="EPSG:32611",
        transform=T,
        nodata=-9999,
        tiled=True,
        blockxsize=64,
        blockysize=32,
        compress="deflate",
    )
    with rio.open(path, "w", **profile) as dst:
        dst.write(data)
        dst.update_tags(source="test", note="a<b&c")
        dst.update_tags(1, band="one")
        dst.set_band_description(1, "elevation")
        dst.set_band_unit(1, "m")
        dst.scales = (0.5, 1.0)
        dst.offsets = (10.0, 0.0)
        dst.build_overviews([2, 4], Resampling.nearest)
    return path, data


def test_open_metadata(dem):
    path, _ = dem
    with rio.open(path) as ds:
        assert not ds.closed and ds.mode == "r" and ds.driver == "GTiff"
        assert (ds.width, ds.height, ds.count, ds.shape) == (301, 203, 2, (203, 301))
        assert ds.dtypes == ("int16", "int16")
        assert ds.nodata == -9999 and ds.nodatavals == (-9999, -9999)
        assert ds.crs.to_epsg() == 32611 and ds.crs == "EPSG:32611"
        assert ds.transform == T
        assert ds.bounds == (500000, 4100000 - 203 * 30, 500000 + 301 * 30, 4100000)
        assert ds.res == (30.0, 30.0)
        assert ds.indexes == (1, 2)
        assert ds.block_shapes == [(32, 64), (32, 64)]
        assert ds.overviews(1) == [2, 4]
        assert ds.tags() == {"source": "test", "note": "a<b&c", "AREA_OR_POINT": "Area"}
        assert ds.tags(1) == {"band": "one"}
        assert ds.descriptions == ("elevation", None)
        assert ds.units == ("m", None)
        assert ds.scales == (0.5, 1.0) and ds.offsets == (10.0, 0.0)
        assert ds.colorinterp == (ColorInterp.gray, ColorInterp.undefined)
        assert ds.mask_flag_enums == ([MaskFlags.nodata], [MaskFlags.nodata])
        p = ds.profile
        assert p["blockxsize"] == 64 and p["blockysize"] == 32 and p["tiled"] is True
        assert p["compress"] == "deflate" and p["interleave"] == "pixel"
        assert ds.meta["count"] == 2
        with pytest.raises(ValueError):
            ds.colormap(1)
    assert ds.closed


def test_reads(dem):
    path, data = dem
    with rio.open(path) as ds:
        assert np.array_equal(ds.read(), data)
        assert np.array_equal(ds.read(2), data[1])
        assert np.array_equal(ds.read([2, 1]), data[[1, 0]])
        w = Window(5, 7, 40, 30)
        assert np.array_equal(ds.read(1, window=w), data[0, 7:37, 5:45])
        assert np.array_equal(ds.read(1, window=((7, 37), (5, 45))), data[0, 7:37, 5:45])
        m = ds.read(1, masked=True)
        assert m.mask.sum() == 10 * 30 and m.fill_value == -9999
        b = ds.read(1, window=Window(-5, -5, 20, 20), boundless=True)
        assert b.shape == (20, 20) and (b[:5] == -9999).all()
        assert np.array_equal(b[5:, 5:], data[0, :15, :15])
        assert ds.read(1, out_shape=(101, 150)).shape == (101, 150)
        up = ds.read(1, window=Window(0, 0, 2, 2), out_shape=(4, 4))
        assert np.array_equal(up, np.repeat(np.repeat(data[0, :2, :2], 2, 0), 2, 1))
        assert ds.read(1, out_dtype="float32").dtype == np.float32
        assert ds.read_masks(1)[15, 40] == 0 and ds.read_masks(1)[0, 0] == 255
        assert ds.dataset_mask().shape == (203, 301)
        with pytest.raises(IndexError):
            ds.read(3)
        with pytest.raises(ValueError):
            ds.read([])


def test_sample_index_xy(dem):
    path, data = dem
    with rio.open(path) as ds:
        x, y = ds.xy(10, 20)
        assert (x, y) == (500000 + 20.5 * 30, 4100000 - 10.5 * 30)
        assert ds.index(x, y) == (10, 20)
        vals = list(ds.sample([(x, y), (0, 0)]))
        assert vals[0].tolist() == data[:, 10, 20].tolist()
        assert vals[1].tolist() == [-9999, -9999]
        w = ds.window(500300, 4099100, 501200, 4099700)
        assert w == Window(10.0, 10.0, 30.0, 20.0)
        assert ds.window_transform(w) == Affine(30, 0, 500300, 0, -30, 4099700)
        assert ds.window_bounds(w) == (500300, 4099100, 501200, 4099700)
        blocks = list(ds.block_windows(1))
        assert blocks[0] == ((0, 0), Window(0, 0, 64, 32))
        assert len(blocks) == 7 * 5


def test_write_roundtrip_profiles(tmp_path):
    data = np.arange(3 * 50 * 60, dtype="uint8").reshape(3, 50, 60)
    path = str(tmp_path / "rgb.tif")
    with rio.open(
        path,
        "w",
        driver="GTiff",
        width=60,
        height=50,
        count=3,
        dtype="uint8",
        crs="EPSG:4326",
        transform=rio.transform.from_origin(0, 50, 1, 1),
    ) as dst:
        dst.write(data)
    with rio.open(path) as ds:
        assert ds.colorinterp == (ColorInterp.red, ColorInterp.green, ColorInterp.blue)
        assert ds.profile["interleave"] == "pixel" and ds.profile["blockysize"] == 45
        assert np.array_equal(ds.read(), data)

    path = str(tmp_path / "band.tif")
    with rio.open(
        path,
        "w",
        driver="GTiff",
        width=60,
        height=50,
        count=2,
        dtype="float32",
        nodata=np.nan,
        interleave="band",
        tiled=True,
        blockxsize=32,
        blockysize=16,
        compress="lzw",
        predictor=3,
    ) as dst:
        dst.write(data[:2].astype("float32"))
    with rio.open(path) as ds:
        p = ds.profile
        assert (p["interleave"], p["blockxsize"], p["blockysize"], p["compress"]) == (
            "band",
            32,
            16,
            "lzw",
        )
        assert ds.tags(ns="IMAGE_STRUCTURE")["PREDICTOR"] == "3"
        assert math.isnan(ds.nodata)
        assert np.array_equal(ds.read(), data[:2].astype("float32"))


def test_write_misuse(tmp_path):
    path = str(tmp_path / "x.tif")
    with pytest.raises(TypeError):
        rio.open(path, "w", driver="GTiff", height=10, count=1, dtype="uint8")
    with pytest.raises(TypeError):
        rio.open(path, "w", driver="GTiff", width=10, height=10, count=1, dtype="foo")
    with pytest.raises(ValueError):
        rio.open(path, "w", driver="GTiff", width=10, height=10, count=1, dtype="uint8", nodata=300)
    with pytest.raises(RasterBlockError):
        rio.open(
            path,
            "w",
            driver="GTiff",
            width=40,
            height=40,
            count=1,
            dtype="uint8",
            tiled=True,
            blockxsize=10,
            blockysize=10,
        )
    with pytest.raises(ValueError):
        rio.open(path, "x")
    with pytest.raises(RasterioIOError):
        rio.open(str(tmp_path / "missing.tif"))
    with rio.open(path, "w", driver="GTiff", width=4, height=4, count=1, dtype="uint8") as dst:
        with pytest.raises(UnsupportedOperation):
            dst.read()
        with pytest.raises(ValueError):
            dst.write(np.zeros((2, 4, 4), "uint8"))


def test_update_and_colormap(dem, tmp_path):
    path, data = dem
    with rio.open(path, "r+") as ds:
        ds.write(np.full((10, 10), 7, "int16"), 1, window=Window(0, 0, 10, 10))
        ds.update_tags(edited="yes")
    with rio.open(path) as ds:
        assert (ds.read(1, window=Window(0, 0, 10, 10)) == 7).all()
        assert ds.tags()["edited"] == "yes" and ds.overviews(1) == [2, 4]
        assert np.array_equal(ds.read(2), data[1])

    path = str(tmp_path / "pal.tif")
    with rio.open(path, "w", driver="GTiff", width=8, height=8, count=1, dtype="uint8") as dst:
        dst.write(np.arange(64, dtype="uint8").reshape(1, 8, 8))
        dst.write_colormap(1, {0: (255, 0, 0), 1: (0, 255, 0, 128)})
    with rio.open(path) as ds:
        cmap = ds.colormap(1)
        assert cmap[0] == (255, 0, 0, 255) and cmap[1] == (0, 255, 0, 255) and len(cmap) == 256
        assert ds.colorinterp == (ColorInterp.palette,)
        assert ds.transform == Affine.identity()


def test_memoryfile_and_file_objects(dem):
    path, data = dem
    with open(path, "rb") as f:
        raw = f.read()
    with rio.MemoryFile(raw) as mem, mem.open() as ds:
        assert np.array_equal(ds.read(), data)
    with rio.MemoryFile() as mem:
        with mem.open(driver="GTiff", width=10, height=5, count=1, dtype="uint8") as dst:
            dst.write(np.ones((1, 5, 10), "uint8"))
        assert len(mem) > 0
        buf = mem.read()
    with rio.open(io.BytesIO(buf)) as ds:
        assert ds.read().sum() == 50


def test_transform_and_windows():
    t = rio.transform.from_bounds(0, 0, 100, 50, 200, 100)
    assert t == Affine(0.5, 0, 0, 0, -0.5, 50)
    assert rio.transform.rowcol(t, 10.2, 40.1) == (19, 20)
    assert rio.transform.xy(t, 0, 0, offset="ul") == (0.0, 50.0)
    assert rio.transform.array_bounds(100, 200, t) == (0, 0, 100, 50)
    with pytest.raises(TypeError):
        rio.transform.guard_transform((0, 1, 0, 0, 0, -1))
    a, b = Window(0, 0, 10, 10), Window(5, 5, 10, 10)
    assert rio.windows.intersection(a, b) == Window(5, 5, 5, 5)
    assert rio.windows.union(a, b) == Window(0, 0, 15, 15)
    assert not rio.windows.intersect(a, Window(20, 20, 1, 1))
    with pytest.raises(WindowError):
        rio.windows.intersection(a, Window(20, 20, 1, 1))
    w = Window(1.6, 2.2, 3.4, 4.6)
    assert w.round_offsets() == Window(1, 2, 3.4, 4.6)
    assert w.round_lengths() == Window(1.6, 2.2, 3, 5)
    assert w.toslices() == (slice(2, 7), slice(1, 5))
    assert Window.from_slices((-3, None), (0, 5), height=10, width=10) == Window(0, 7, 5, 3)


def test_crs():
    c = rio.CRS.from_epsg(32611)
    assert c.to_epsg() == 32611 and c.to_string() == "EPSG:32611"
    assert c.is_projected and not c.is_geographic
    assert c.linear_units == "metre" and c.linear_units_factor == ("metre", 1.0)
    assert c.to_dict() == {
        "proj": "utm",
        "zone": 11,
        "datum": "WGS84",
        "units": "m",
        "no_defs": True,
    }
    g = rio.CRS.from_user_input("EPSG:4326")
    assert g.is_geographic and g == 4326 and g != c
    assert g.units_factor == ("degree", math.pi / 180)
    assert (
        rio.CRS.from_string("+proj=utm +zone=13 +datum=WGS84 +units=m +no_defs").to_epsg() == 32613
    )
    assert rio.CRS.from_wkt(c.to_wkt()) == c
    with pytest.raises(CRSError):
        rio.CRS.from_string("not a crs")
    with pytest.raises(CRSError):
        _ = g.linear_units_factor


def test_features():
    geoms = [
        {
            "type": "Polygon",
            "coordinates": [
                [
                    (500000, 4100000),
                    (500300, 4100000),
                    (500300, 4099700),
                    (500000, 4099700),
                    (500000, 4100000),
                ]
            ],
        },
        ({"type": "Point", "coordinates": (500615, 4099385)}, 5),
    ]
    a = features.rasterize(geoms, out_shape=(30, 30), transform=T)
    assert a.dtype == np.int64 and a[:10, :10].sum() == 100 and a[20, 20] == 5
    m = features.geometry_mask([geoms[0]], (30, 30), T, invert=True)
    assert m.dtype == bool and m.sum() == 100
    arr = np.zeros((6, 6), "uint8")
    arr[1:3, 1:4] = 2
    shapes = list(features.shapes(arr, transform=T))
    assert sorted(v for _, v in shapes) == [0.0, 2.0]
    assert features.bounds(geoms[0]) == (500000, 4099700, 500300, 4100000)


def test_warp_merge_mask(dem, tmp_path):
    path, data = dem
    with rio.open(path) as ds:
        t, w, h = warp.calculate_default_transform(
            ds.crs, "EPSG:4326", ds.width, ds.height, *ds.bounds
        )
        assert t.a > 0 and w > 0 and h > 0
        dst = np.zeros((2, h, w), "int16")
        warp.reproject(rio.band(ds, [1, 2]), dst, dst_transform=t, dst_crs="EPSG:4326")
        assert (dst != 0).any()
        out, ot = warp.reproject(
            ds.read(1),
            src_transform=ds.transform,
            src_crs=ds.crs,
            dst_crs="EPSG:4326",
            src_nodata=-9999,
        )
        assert out.shape[1:] == (h, w) and ot == t
        xs, ys = warp.transform("EPSG:32611", "EPSG:4326", [500000], [4100000])
        assert abs(xs[0] + 117) < 1e-9 and 37 < ys[0] < 37.1
        with rio.vrt.WarpedVRT(ds, crs="EPSG:4326") as vrt:
            assert vrt.shape == (h, w) and vrt.read().shape == (2, h, w)
        merged, mt = merge.merge([ds, ds], res=60)
        assert merged.shape == (2, 102, 150) and mt.a == 60
        poly = {
            "type": "Polygon",
            "coordinates": [
                [(500300, 4099700), (501000, 4099700), (501000, 4099000), (500300, 4099700)]
            ],
        }
        masked, transform = rio.mask.mask(ds, [poly], crop=True)
        assert masked.shape[0] == 2 and transform.c == 500300


def test_env_and_session():
    import os

    with rio.Env(GDAL_HTTP_TIMEOUT=17, AWS_NO_SIGN_REQUEST=True) as env:
        assert os.environ["GDAL_HTTP_TIMEOUT"] == "17"
        assert os.environ["AWS_NO_SIGN_REQUEST"] == "ON"
        assert rio.env.getenv()["GDAL_HTTP_TIMEOUT"] == 17
        assert env.drivers()
    assert "GDAL_HTTP_TIMEOUT" not in os.environ or os.environ["GDAL_HTTP_TIMEOUT"] != "17"
    s = rio.session.AWSSession(
        aws_access_key_id="a", aws_secret_access_key="b", region_name="us-west-2"
    )
    assert s.get_credential_options() == {
        "AWS_ACCESS_KEY_ID": "a",
        "AWS_SECRET_ACCESS_KEY": "b",
        "AWS_REGION": "us-west-2",
    }


def test_plot(dem):
    plt = pytest.importorskip("matplotlib.pyplot")
    import matplotlib

    matplotlib.use("Agg")
    from terrio.rio import plot

    path, _ = dem
    with rio.open(path) as ds, warnings.catch_warnings():
        warnings.simplefilter("ignore")
        fig, ax = plt.subplots()
        assert plot.show(ds, ax=ax, title="dem") is ax
        plot.show_hist(ds, ax=ax)
        plt.close(fig)


def test_profile_classes():
    p = rio.profiles.DefaultGTiffProfile(count=3)
    assert p["tiled"] is True and p["count"] == 3 and p["dtype"] == "uint8"
    with pytest.raises(TypeError):
        p["affine"] = Affine.identity()
    assert json.dumps(dict(p))


def test_rotated_gcps_rpcs_masks(tmp_path):
    rot = Affine(10, 2, 500000, 1.5, -10, 4100000)
    data = (np.arange(3 * 40 * 50).reshape(3, 40, 50) % 251).astype("uint8")
    path = str(tmp_path / "rot.tif")
    with rio.open(path, "w", width=50, height=40, count=3, dtype="uint8", crs="EPSG:32611", transform=rot) as dst:
        dst.write(data)
    with rio.open(path) as ds:
        assert ds.transform == rot and np.array_equal(ds.read(), data)
        assert ds.res == pytest.approx((math.hypot(10, 1.5), math.hypot(2, 10)))
    from terrio.rio.control import GroundControlPoint
    from terrio.rio.rpc import RPC

    gcps = [GroundControlPoint(row=r, col=c, x=500000 + c * 10, y=4100000 - r * 10) for r in (0, 40) for c in (0, 50)]
    path = str(tmp_path / "gcps.tif")
    with rio.open(path, "w", width=50, height=40, count=1, dtype="uint8", gcps=gcps, crs="EPSG:32611") as dst:
        dst.write(data[:1])
    with rio.open(path) as ds:
        got, crs = ds.gcps
        assert [(g.row, g.col, g.x, g.y) for g in got] == [(g.row, g.col, g.x, g.y) for g in gcps]
        assert crs.to_epsg() == 32611 and ds.crs is None
        out, t = warp.reproject(rio.band(ds, 1), dst_crs="EPSG:32611")
        assert out.shape[-2:] == (40, 50) and t.a == pytest.approx(10.0)
    vals = [float(i) for i in range(92)]
    path = str(tmp_path / "rpc.tif")
    with rio.open(path, "w", driver="GTiff", width=5, height=4, count=1, dtype="uint8") as dst:
        dst.write(np.zeros((1, 4, 5), "uint8"))
        dst.rpcs = RPC.from_tiff_tag(vals)
    with rio.open(path) as ds:
        assert ds.rpcs.to_tiff_tag() == vals
    mask = np.full((40, 50), 255, np.uint8)
    mask[5:9, 3:20] = 0
    path = str(tmp_path / "mask.tif")
    with rio.open(path, "w", width=50, height=40, count=3, dtype="uint8", tiled=True, blockxsize=16, blockysize=16) as dst:
        dst.write(data)
        dst.write_mask(mask)
    with rio.open(path) as ds:
        assert ds.mask_flag_enums[0] == [MaskFlags.per_dataset]
        assert np.array_equal(ds.dataset_mask(), mask)
        assert ds.read(1, masked=True).mask.sum() == (mask == 0).sum()


def test_per_band_nodata_shutil_stack(tmp_path):
    from terrio.rio import shutil as rshutil
    from terrio.rio.stack import stack

    path = str(tmp_path / "a.tif")
    data = np.arange(2 * 10 * 12, dtype="uint8").reshape(2, 10, 12)
    with rio.open(path, "w", width=12, height=10, count=2, dtype="uint8", crs="EPSG:32611", transform=T) as dst:
        dst.write(data)
    with open(path + ".aux.xml", "w") as f:
        f.write('<PAMDataset><PAMRasterBand band="2"><NoDataValue>130</NoDataValue></PAMRasterBand></PAMDataset>')
    with rio.open(path) as ds:
        assert ds.nodatavals == (None, 130.0)
        assert ds.read(masked=True).mask.sum() == 1
    assert rshutil.exists(path) and not rshutil.exists(str(tmp_path / "no.tif"))
    copy = str(tmp_path / "b.tif")
    rshutil.copy(path, copy, compress="deflate")
    with rio.open(copy) as ds:
        assert np.array_equal(ds.read(), data) and ds.compression.name == "deflate"
    rshutil.copyfiles(path, str(tmp_path / "c.tif"))
    assert (tmp_path / "c.tif.aux.xml").exists()
    rshutil.delete(str(tmp_path / "c.tif"))
    assert not (tmp_path / "c.tif").exists() and not (tmp_path / "c.tif.aux.xml").exists()
    with pytest.raises(RasterioIOError):
        rshutil.delete(str(tmp_path / "c.tif"))
    arr, t = stack([path, copy], indexes=[1, [1, 2]])
    assert arr.shape == (3, 10, 12) and t == T
    assert np.array_equal(arr[1:], data)


def test_streaming_writes(tmp_path, monkeypatch):
    monkeypatch.setenv("TERRIO_RIO_IN_MEMORY_LIMIT", "1000")
    data = (np.arange(2 * 300 * 400) % 65000).astype("uint16").reshape(2, 300, 400)
    mask = np.full((300, 400), 255, np.uint8)
    mask[100:110] = 0
    for kw in ({}, dict(tiled=True, blockxsize=64, blockysize=32, compress="deflate"), dict(interleave="band", compress="lzw")):
        path = str(tmp_path / "s.tif")
        with rio.open(path, "w", width=400, height=300, count=2, dtype="uint16", crs="EPSG:32611", transform=T, nodata=7, **kw) as dst:
            assert isinstance(dst._data, np.memmap)
            for y in range(0, 300, 70):
                dst.write(data[:, y : y + 70], window=Window(0, y, 400, min(70, 300 - y)))
            dst.write_mask(mask)
        assert not [p for p in tmp_path.iterdir() if "terrio-" in p.name]
        with rio.open(path) as ds:
            assert np.array_equal(ds.read(), data) and ds.nodata == 7
            assert np.array_equal(ds.dataset_mask(), mask)
        with rio.open(path, "r+") as ds:
            ds.write(np.zeros((2, 5, 5), "uint16"), window=Window(0, 0, 5, 5))
        with rio.open(path) as ds:
            assert ds.read(1, window=Window(0, 0, 6, 6))[:, 5].tolist() == data[0, :6, 5].tolist()
            assert np.array_equal(ds.dataset_mask(), mask)
