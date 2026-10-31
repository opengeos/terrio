"""`terrio.rio.vrt.WarpedVRT` against rasterio's `WarpedVRT` (GDAL's warped VRT).

The sources are written by GDAL (through rasterio) with overviews, so the implicit overviews, the
overview a read chooses and the warp of each read are GDAL's own. Skipped without rasterio.
"""

import math
import re

import numpy as np
import pytest

rasterio = pytest.importorskip("rasterio")

from affine import Affine  # noqa: E402
from rasterio.enums import Resampling as RR  # noqa: E402
from rasterio.vrt import WarpedVRT as RasterioVRT  # noqa: E402
from rasterio.windows import Window as RW  # noqa: E402

import terrio.rio as trio  # noqa: E402
from terrio.rio.enums import Resampling as TR  # noqa: E402
from terrio.rio.vrt import WarpedVRT  # noqa: E402
from terrio.rio.windows import Window as TW  # noqa: E402


@pytest.fixture(scope="module")
def utm(tmp_path_factory):
    """A 3-band uint16 UTM GeoTIFF with nodata 0 (a border and a hole) and overviews."""
    path = str(tmp_path_factory.mktemp("wvrt") / "utm.tif")
    rng = np.random.default_rng(3)
    yy, xx = np.mgrid[0:700, 0:900]
    base = (np.sin(xx / 23.0) + np.cos(yy / 31.0)) * 2000 + 5000
    data = np.stack([base + rng.normal(0, 300, base.shape) + 400 * b for b in range(3)])
    data = np.clip(data, 1, 65535).astype("uint16")
    data[:, :, :40] = 0
    data[:, 300:340, 500:560] = 0
    with rasterio.open(
        path,
        "w",
        driver="GTiff",
        width=900,
        height=700,
        count=3,
        dtype="uint16",
        nodata=0,
        crs="EPSG:32613",
        transform=Affine(10, 0, 400000, 0, -10, 4500000),
        tiled=True,
        blockxsize=256,
        blockysize=256,
    ) as dst:
        dst.write(data)
        dst.build_overviews([2, 4, 8], RR.average)
    return path


@pytest.fixture(scope="module")
def rgb(tmp_path_factory):
    """An RGB uint8 GeoTIFF without nodata, with an internal mask and overviews."""
    path = str(tmp_path_factory.mktemp("wvrt") / "rgb.tif")
    yy, xx = np.mgrid[0:600, 0:640]
    data = np.stack([(xx * 3 + yy) % 251, (xx + yy * 5) % 241, (xx * 7 + yy * 2) % 239])
    mask = np.full((600, 640), 255, "uint8")
    mask[:, :90] = 0
    mask[400:450, 300:380] = 0
    with rasterio.Env(GDAL_TIFF_INTERNAL_MASK=True):
        with rasterio.open(
            path,
            "w",
            driver="GTiff",
            width=640,
            height=600,
            count=3,
            dtype="uint8",
            crs="EPSG:3857",
            transform=Affine(2, 0, -11800000, 0, -2, 4900000),
            tiled=True,
            blockxsize=128,
            blockysize=128,
            photometric="RGB",
        ) as dst:
            dst.write(data.astype("uint8"))
            dst.write_mask(mask)
            dst.build_overviews([2, 4], RR.nearest)
    return path


# A tile-like grid in Web Mercator over part of the UTM raster (as a tile server asks for them),
# and GDAL's default grid (no transform or size given).
TILE = dict(
    crs="EPSG:3857",
    transform=Affine(13.0, 0, -11820000.0, 0, -13.0, 4958000.0),
    width=600,
    height=500,
)
DEFAULT = dict(crs="EPSG:3857")


def _pair(path, **kw):
    """The same WarpedVRT opened with rasterio and with terrio."""
    r_kw = dict(kw)
    t_kw = dict(kw)
    if "resampling" in kw:
        r_kw["resampling"] = RR[kw["resampling"]]
        t_kw["resampling"] = TR[kw["resampling"]]
    rs = rasterio.open(path)
    ts = trio.open(path)
    return rs, RasterioVRT(rs, **r_kw), ts, WarpedVRT(ts, **t_kw)


@pytest.mark.parametrize("grid", [TILE, DEFAULT], ids=["tile", "default"])
def test_implicit_overviews_are_gdals(utm, grid):
    """The VRT's overviews are built from the source's as GDAL builds them."""
    rs, rv, ts, tv = _pair(utm, **grid)
    with rs, rv, ts, tv:
        assert (tv.width, tv.height) == (rv.width, rv.height)
        # GDAL's suggested grid is computed through PROJ: the same to the last digits or so.
        assert tv.transform.almost_equals(rv.transform, precision=1e-6)
        assert tv.overviews(1) == rv.overviews(1)
        # The sizes behind the factors: reading each overview's size exactly selects it.
        for ow, oh in tv._overview_sizes:
            a = rv.read(1, out_shape=(oh, ow))
            b = tv.read(1, out_shape=(oh, ow))
            assert np.array_equal(a, b), (ow, oh)


READS = [
    ("whole", None, None),
    ("whole decimated", None, (256, 256)),
    ("whole decimated, odd size", None, (91, 77)),
    ("whole enlarged", None, (900, 800)),
    ("window", ((100, 356), (40, 296)), None),
    ("window decimated", ((10, 460), (5, 505)), (128, 128)),
    ("fractional window resampled", ((100.25, 300.5), (40.75, 290.0)), (99, 101)),
    ("small window", ((50, 60), (70, 90)), None),
]


@pytest.mark.parametrize("resampling", ["nearest", "bilinear", "cubic"])
@pytest.mark.parametrize("name,window,shape", READS, ids=[r[0] for r in READS])
def test_reads_match_rasterio(utm, resampling, name, window, shape):
    """Reads (whole or windowed, at the VRT's size or another) give GDAL's pixels: the same
    overview, the same regions warped, the same resampling."""
    rs, rv, ts, tv = _pair(utm, resampling=resampling, nodata=0, src_nodata=0, **TILE)
    with rs, rv, ts, tv:
        kw_r, kw_t = {}, {}
        if window is not None:
            (r0, r1), (c0, c1) = window
            kw_r["window"] = RW(c0, r0, c1 - c0, r1 - r0)
            kw_t["window"] = TW(c0, r0, c1 - c0, r1 - r0)
        if shape is not None:
            kw_r["out_shape"] = kw_t["out_shape"] = (3, *shape)
            kw_r["resampling"] = RR[resampling]
            kw_t["resampling"] = TR[resampling]
        a = rv.read(**kw_r)
        b = tv.read(**kw_t)
        assert a.shape == b.shape
        assert np.array_equal(a, b), f"{(a != b).mean():.4%} of pixels differ"


@pytest.mark.parametrize("shape", [None, (256, 256), (300, 200)])
def test_add_alpha_matches_rasterio(rgb, shape):
    """An added alpha band (the source's internal mask decides validity) is GDAL's, and so are the
    bands read with it, as a tile server reads them."""
    grid = dict(crs="EPSG:4326", add_alpha=True)
    rs, rv, ts, tv = _pair(rgb, **grid)
    with rs, rv, ts, tv:
        assert tv.count == rv.count == 4
        assert tv.colorinterp == rv.colorinterp
        assert [f.value for f in tv.mask_flag_enums[0]] == [f.value for f in rv.mask_flag_enums[0]]
        assert tv.nodata is None and rv.nodata is None
        kw = {} if shape is None else {"out_shape": (4, *shape)}
        a = rv.read(indexes=(1, 2, 3, 4), **kw)
        b = tv.read(indexes=(1, 2, 3, 4), **kw)
        assert np.array_equal(a, b), f"{(a != b).mean():.4%} of pixels differ"
        assert (b[3] == 0).any() and (b[3] == 255).any()


def test_source_is_never_read_whole(utm):
    """Reads warp from the source windows (of the chosen level) they need: the source is never
    read whole, through `read` or a full-size window."""
    with trio.open(utm) as ds:
        windows = []
        level_read = ds._read_level

        def spy(bands, window, level):
            windows.append((window, level))
            return level_read(bands, window, level)

        def no_read(*args, **kwargs):
            raise AssertionError("the source was read through read()")

        ds._read_level = spy
        ds.read = no_read
        with WarpedVRT(ds, nodata=0, src_nodata=0, **TILE) as vrt:
            vrt.read(out_shape=(3, 128, 128))
            vrt.read(window=TW(10, 20, 200, 100))
            vrt.read()
        assert windows
        for (x, y, w, h), level in windows:
            full = (ds.width, ds.height) if level is None else ds._overview_sizes[level]
            assert w * h < full[0] * full[1], (x, y, w, h, level)
        # The decimated read used an overview of the source.
        assert any(level is not None for _, level in windows)


@pytest.fixture(scope="module", params=["plain", "overviews"])
def gcp_file(request, tmp_path_factory):
    """A uint16 GeoTIFF georeferenced by GCPs alone (no geotransform), rotated a little, with and
    without overviews."""
    from rasterio.control import GroundControlPoint

    path = str(tmp_path_factory.mktemp("wvrt") / f"gcps_{request.param}.tif")
    gcps = [
        GroundControlPoint(row=r, col=c, x=500000 + 10 * c + 0.5 * r, y=4000000 - 10 * r + 0.3 * c)
        for r in (0, 200, 399)
        for c in (0, 300, 599)
    ]
    data = ((np.arange(600 * 400).reshape(1, 400, 600) * 7) % 4093 + 1).astype("uint16")
    with rasterio.open(
        path,
        "w",
        driver="GTiff",
        width=600,
        height=400,
        count=1,
        dtype="uint16",
        gcps=gcps,
        crs="EPSG:32631",
    ) as dst:
        dst.write(data)
        if request.param == "overviews":
            dst.build_overviews([2, 4], RR.average)
    return path


@pytest.mark.parametrize("shape", [None, (128, 192), (50, 76)])
def test_gcp_source_matches_rasterio(gcp_file, shape):
    """A tile server opens a GCP-referenced asset as `WarpedVRT(src, src_crs=<the GCPs' CRS>,
    src_transform=from_gcps(...), add_alpha=True)`. GDAL then grids and warps with the GCPs
    themselves (`GDALAutoCreateWarpedVRTEx`), not with `src_transform`; reduced reads go through
    warped blocks (a GCP transformer has no fast clone), from overviews when the source has them."""
    kw = {} if shape is None else {"out_shape": (2, *shape)}
    out = []
    for lib, vrt_class in ((rasterio, RasterioVRT), (trio, WarpedVRT)):
        with lib.open(gcp_file) as src:
            gcps, crs = src.gcps
            options = dict(src_crs=crs, src_transform=lib.transform.from_gcps(gcps), add_alpha=True)
            with vrt_class(src, **options) as vrt:
                out.append(
                    (
                        (vrt.width, vrt.height),
                        tuple(vrt.transform)[:6],
                        vrt.crs.to_epsg(),
                        vrt.read(**kw),
                    )
                )
    (g_size, g_t, g_crs, a), (t_size, t_t, t_crs, b) = out
    assert (t_size, t_crs) == (g_size, g_crs)
    assert np.allclose(t_t, g_t, rtol=0, atol=1e-6), (t_t, g_t)
    assert a.shape == b.shape
    assert np.array_equal(a, b), f"{(a != b).mean():.4%} of pixels differ"


def test_rotated_source_grid_matches_rasterio(tmp_path):
    """GDAL suggests the default grid from the source's geotransform, rotation included (its
    bounds leave the rotation out)."""
    path = str(tmp_path / "rotated.tif")
    data = ((np.arange(600 * 400).reshape(1, 400, 600) * 7) % 4093 + 1).astype("uint16")
    with rasterio.open(
        path,
        "w",
        driver="GTiff",
        width=600,
        height=400,
        count=1,
        dtype="uint16",
        crs="EPSG:32631",
        transform=Affine(10, 0.5, 500000, 0.3, -10, 4000000),
    ) as dst:
        dst.write(data)
    for options in ({}, {"crs": "EPSG:4326"}):
        out = []
        for lib, vrt_class in ((rasterio, RasterioVRT), (trio, WarpedVRT)):
            with lib.open(path) as src, vrt_class(src, **options) as vrt:
                out.append(((vrt.width, vrt.height), vrt.read()))
        (g_size, a), (t_size, b) = out
        assert t_size == g_size, options
        assert np.array_equal(a, b), options


# RPCs of a 240 x 200 image near (115.2 W, 35.6 N): the merged suite's `SYNTH_RPC`
# (`scripts/compare_rio_with_rasterio.py`).
SYNTH_RPC = {
    "LINE_OFF": "100",
    "SAMP_OFF": "120",
    "LAT_OFF": "35.6",
    "LONG_OFF": "-115.2",
    "HEIGHT_OFF": "800",
    "LINE_SCALE": "100",
    "SAMP_SCALE": "120",
    "LAT_SCALE": "0.05",
    "LONG_SCALE": "0.06",
    "HEIGHT_SCALE": "1000",
    "LINE_NUM_COEFF": (
        "0.002 0.005 -1 0.06 0.004 0.001 -0.002 0.001 -0.01 0 0.0005 0 0.0002 0 0.0001 0 0 0 0 0"
    ),
    "LINE_DEN_COEFF": "1 0.001 -0.002 0.0005 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0",
    "SAMP_NUM_COEFF": "-0.001 1 0.02 0.09 0.02 0.003 0 0.01 0.002 0 0 0.0003 0 0 0 0 0 0 0 0",
    "SAMP_DEN_COEFF": "1 0.001 -0.002 0.0005 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0",
}


@pytest.fixture(scope="module")
def rpc_dir(tmp_path_factory):
    """Rasters georeferenced by `SYNTH_RPC` alone (no transform, no GCPs), written by GDAL: the
    suite's uint8 image; three bands with nodata 0 (a hole) and overviews; and a WGS 84 DEM
    named in upper case (rasterio upper-cases warp extras, so only such a name survives)."""
    d = tmp_path_factory.mktemp("wvrt-rpc")
    yy, xx = np.mgrid[0:200, 0:240]
    img = ((np.sin(xx / 7.0) + np.cos(yy / 5.0)) * 60 + 128 + (xx * yy) % 17).astype("uint8")
    rpcs = rasterio.rpc.RPC.from_gdal(SYNTH_RPC)
    with rasterio.open(
        d / "image.tif", "w", driver="GTiff", width=240, height=200, count=1, dtype="uint8"
    ) as dst:
        dst.write(img[None])
        dst.rpcs = rpcs
    stack = np.stack([img, ((xx * 3 + yy) % 251).astype("uint8"), (255 - img).astype("uint8")])
    stack[:, 80:100, 100:130] = 0
    with rasterio.open(
        d / "rgb.tif",
        "w",
        driver="GTiff",
        width=240,
        height=200,
        count=3,
        dtype="uint8",
        nodata=0,
        tiled=True,
        blockxsize=64,
        blockysize=64,
    ) as dst:
        dst.write(stack)
        dst.rpcs = rpcs
    with rasterio.open(d / "rgb.tif", "r+") as dst:
        dst.build_overviews([2, 4], RR.average)
    t = rasterio.transform.from_origin(-115.275, 35.645, 0.001, 0.001)
    rows, cols = np.mgrid[0:100, 0:130]
    lon, lat = t.c + (cols + 0.5) * t.a, t.f + (rows + 0.5) * t.e
    z = (800 + 600 * np.sin((lon + 115.2) * 60) * np.cos((lat - 35.6) * 45)).astype("float32")
    z[40:44, 60:70] = -9999
    with rasterio.open(
        d / "DEM.TIF",
        "w",
        driver="GTiff",
        width=130,
        height=100,
        count=1,
        dtype="float32",
        crs="EPSG:4326",
        transform=t,
        nodata=-9999,
    ) as dst:
        dst.write(z[None])
    return d


RPC_CASES = [
    # The merged suite's two (`warp: RPCs ... WarpedVRT by RPCs`).
    ("image.tif", dict(crs="EPSG:4326")),
    ("image.tif", dict(crs="EPSG:32611", RPC_HEIGHT=700, resampling=RR.bilinear)),
    ("image.tif", dict(crs="EPSG:3857", RPC_HEIGHT=300, RPC_HEIGHT_SCALE=2, resampling=RR.average)),
    # A DEM with holes, with a missing value: no point fails. (Where points fail, GDAL's
    # approximate transformer stops at the first failed batch and its kernel reads the rest of
    # the row at stale positions, which is not reproduced.)
    (
        "image.tif",
        dict(
            crs="EPSG:4326",
            RPC_DEM="DEM.TIF",
            RPC_DEMINTERPOLATION="cubic",
            RPC_DEM_MISSING_VALUE=500,
            resampling=RR.cubic,
        ),
    ),
    (
        "image.tif",
        dict(
            crs="EPSG:32611", RPC_DEM="DEM.TIF", RPC_DEM_MISSING_VALUE=500, resampling=RR.bilinear
        ),
    ),
    ("image.tif", dict()),
    ("image.tif", dict(crs="EPSG:4326", add_alpha=True, tolerance=0)),
    ("rgb.tif", dict(crs="EPSG:32611", resampling=RR.bilinear)),
    # rasterio's cases 3 and 1: the grid from the RPCs (or given), the warp by the source
    # transform (an identity, without a CRS: no pixel lands, but on a grid around the source's
    # own pixel coordinates, which are not reprojected).
    ("image.tif", dict(crs="EPSG:4326", width=100, height=90)),
    (
        "image.tif",
        dict(
            crs="EPSG:4326",
            width=120,
            height=100,
            transform=Affine(0.0005, 0, -115.27, 0, -0.0005, 35.65),
        ),
    ),
    ("image.tif", dict(crs="EPSG:3857", width=120, height=100, transform=Affine(2, 0, 0, 0, 2, 0))),
]


@pytest.mark.parametrize(
    "name,options",
    # rgb.tif: three bands with a nodata value.
    [pytest.param(n, o) for n, o in RPC_CASES],
    ids=[f"{n}-{i}" for i, (n, _) in enumerate(RPC_CASES)],
)
def test_rpc_source_matches_rasterio(rpc_dir, monkeypatch, name, options):
    """A source georeferenced by RPCs alone: GDAL grids and warps it with its RPC transformer
    (`GDALAutoCreateWarpedVRTEx`), WGS 84 by default, with the `RPC_*` warp extras; reads at a
    reduced size take the fast-clone path (an RPC transformer has one) or, from a source with
    overviews, the implicit overviews (`GDALCreateSimilarRPCTransformer`)."""
    monkeypatch.chdir(rpc_dir)
    out = []
    for lib, vrt_class, window in ((rasterio, RasterioVRT, RW), (trio, WarpedVRT, TW)):
        with lib.open(rpc_dir / name) as src, vrt_class(src, **options) as vrt:
            h, w = vrt.height, vrt.width
            reads = [
                vrt.read(),
                vrt.read(window=window(w // 5, h // 4, w // 2, h // 3)),
                vrt.read(out_shape=(vrt.count, h // 3, w // 3)),
                vrt.read(out_shape=(vrt.count, h // 2, w // 2), resampling=RR.bilinear),
                vrt.read_masks(),
            ]
            crs = None if vrt.crs is None else vrt.crs.to_epsg()
            out.append(
                (
                    (vrt.count, h, w),
                    tuple(vrt.transform)[:6],
                    crs,
                    vrt.nodata,
                    vrt.overviews(1),
                    reads,
                )
            )
    (g_shape, g_t, g_crs, g_nd, g_ovr, g_reads), (t_shape, t_t, t_crs, t_nd, t_ovr, t_reads) = out
    assert (t_shape, t_crs, t_nd, t_ovr) == (g_shape, g_crs, g_nd, g_ovr)
    np.testing.assert_allclose(t_t, g_t, rtol=1e-12, atol=0)
    for i, (a, b) in enumerate(zip(g_reads, t_reads, strict=True)):
        assert a.shape == b.shape and a.dtype == b.dtype, i
        assert np.array_equal(a, b), (
            f"read {i}: {np.count_nonzero(a != b)} of {a.size} pixels differ"
        )
    if "width" not in options or options.get("crs") == "EPSG:3857":
        # Pixels land: not the empty warps of rasterio's cases 1 and 3 on geographic grids.
        assert np.count_nonzero(t_reads[0]) > t_reads[0].size // 2


@pytest.fixture(scope="module")
def two_band_nodata(tmp_path_factory):
    """A 2-band float64 geographic GeoTIFF with nodata 9999: band 1 has nodata areas where band 2
    is valid; band 2 has nodata only along its north edge, so that a region of the warped VRT
    (whole rows of blocks) south of it has a band without nodata pixels."""
    path = str(tmp_path_factory.mktemp("wvrt") / "two_band_nodata.tif")
    yy, xx = np.mgrid[0:240, 0:320]
    b1 = 200.0 + np.sin(xx / 11.0) * 50 + yy * 0.25
    b2 = 1000.0 + np.cos(yy / 13.0) * 80 + xx * 0.5
    b1[60:120, 100:180] = 9999.0
    b1[200:, 200:] = 9999.0
    b2[:12, :] = 9999.0
    with rasterio.open(
        path,
        "w",
        driver="GTiff",
        width=320,
        height=240,
        count=2,
        dtype="float64",
        nodata=9999.0,
        crs="EPSG:4326",
        transform=Affine(0.05, 0, -100.0, 0, -0.05, 45.0),
        tiled=True,
        blockxsize=128,
        blockysize=128,
    ) as dst:
        dst.write(np.stack([b1, b2]))
    return path


@pytest.mark.parametrize("resampling", ["nearest", "bilinear"])
def test_multiband_nodata_matches_rasterio(two_band_nodata, resampling):
    """Several bands with a nodata value: GDAL 3.12 unifies their validity (rasterio's
    `UNIFIED_SRC_NODATA=YES`: a pixel is valid where any band is, and everywhere in a region
    where some band has no nodata pixel), so a band's nodata values are warped where the pixel
    is valid, and stay at the output nodata value unless every band is at it (GDAL 3.12.2)."""
    rs, rv, ts, tv = _pair(two_band_nodata, crs="EPSG:3857", resampling=resampling)
    with rs, rv, ts, tv:
        h, w = rv.height, rv.width
        assert (tv.count, tv.height, tv.width) == (2, h, w)
        reads = []
        for vrt, window in ((rv, RW), (tv, TW)):
            reads.append(
                [
                    vrt.read(),
                    # South of band 2's nodata: band 2 valid everywhere, every pixel valid.
                    vrt.read(window=window(0, h // 2, w, h - h // 2)),
                    vrt.read(out_shape=(2, h // 3, w // 3)),
                ]
            )
    for i, (a, b) in enumerate(zip(*reads, strict=True)):
        assert a.shape == b.shape and a.dtype == b.dtype, i
        assert np.array_equal(a, b), f"read {i}: {np.count_nonzero(a != b)} of {a.size} differ"


@pytest.mark.parametrize("resampling", ["nearest", "bilinear"])
@pytest.mark.parametrize("crs", ["EPSG:4326", "EPSG:3857"])
def test_output_nodata_without_source_nodata_matches_rasterio(tmp_path, crs, resampling):
    """An output nodata value for a source without one: the warp starts from it (rasterio's
    `INIT_DEST=NO_DATA`), so GDAL 3.12 makes an output validity mask and moves warped values
    equal to it off it, 255 to 254."""
    path = str(tmp_path / "u8.tif")
    yy, xx = np.mgrid[0:200, 0:240]
    img = ((xx * 3 + yy * 5) % 256).astype("uint8")
    with rasterio.open(
        path,
        "w",
        driver="GTiff",
        width=240,
        height=200,
        count=1,
        dtype="uint8",
        crs="EPSG:4326",
        transform=Affine(0.0005, 0, -115.26, 0, -0.0005, 35.65),
    ) as dst:
        dst.write(img[np.newaxis])
    rs, rv, ts, tv = _pair(path, crs=crs, nodata=255, resampling=resampling)
    with rs, rv, ts, tv:
        h, w = rv.height, rv.width
        reads = []
        for vrt, window in ((rv, RW), (tv, TW)):
            reads.append(
                [
                    vrt.read(),
                    vrt.read(window=window(w // 5, h // 4, w // 2, h // 3)),
                    vrt.read(out_shape=(1, h // 3, w // 3)),
                    vrt.read_masks(),
                ]
            )
    for i, (a, b) in enumerate(zip(*reads, strict=True)):
        assert a.shape == b.shape and a.dtype == b.dtype, i
        assert np.array_equal(a, b), f"read {i}: {np.count_nonzero(a != b)} of {a.size} differ"
    if crs == "EPSG:4326" and resampling == "nearest":
        # The source's own grid: every 255 is moved to 254.
        assert np.array_equal(reads[1][0], np.where(img == 255, 254, img)[np.newaxis])


@pytest.mark.parametrize(
    "crs",
    [
        "EPSG:3857",
        pytest.param(
            "EPSG:32611",
            marks=pytest.mark.xfail(
                reason="the warper's source coordinates differ from GDAL's by up to 5e-11 px "
                "through this projection, which moves some float32 values by an ULP"
            ),
        ),
    ],
)
@pytest.mark.parametrize("resampling", ["bilinear", "cubic"])
def test_float32_reads_match_rasterio(tmp_path, crs, resampling):
    """Random float32 values, where the last bit of the kernels' and the coordinates' rounding
    shows: GDAL's values through Web Mercator; through UTM, some pixels differ by an ULP."""
    path = str(tmp_path / "f32.tif")
    values = np.random.default_rng(3).uniform(-1000, 1000, (1, 300, 400)).astype("float32")
    with rasterio.open(
        path,
        "w",
        driver="GTiff",
        width=400,
        height=300,
        count=1,
        dtype="float32",
        crs="EPSG:4326",
        transform=Affine(0.0005, 0, -115.26, 0, -0.0005, 35.65),
    ) as dst:
        dst.write(values)
    rs, rv, ts, tv = _pair(path, crs=crs, resampling=resampling)
    with rs, rv, ts, tv:
        h, w = rv.height, rv.width
        reads = []
        for vrt, window in ((rv, RW), (tv, TW)):
            reads.append([vrt.read(), vrt.read(window=window(w // 5, h // 4, w // 2, h // 3))])
    for i, (a, b) in enumerate(zip(*reads, strict=True)):
        assert a.shape == b.shape and a.dtype == b.dtype, i
        assert np.array_equal(a, b), f"read {i}: {np.count_nonzero(a != b)} of {a.size} differ"


@pytest.fixture(scope="module")
def two_band_edges(tmp_path_factory):
    """Two Float64 bands, nodata 9999: band 1's in rows 60-99 from column 80, band 2's in the
    first 12 columns, so the warped blocks south of band 1's nodata rows hold no band-1 nodata
    pixel while band 2's run down their west edge."""
    path = str(tmp_path_factory.mktemp("band-subsets") / "two.tif")
    y, x = np.mgrid[0:240, 0:320].astype("float64")
    b1 = 150.0 + 80.0 * (np.sin(x / 23.0) * np.cos(y / 31.0) + 1.0)
    b2 = 926.0 + 150.0 * (np.cos(x / 41.0 + y / 17.0) + 1.0)
    b1[60:100, 80:320] = 9999.0
    b2[:, :12] = 9999.0
    with rasterio.open(
        path,
        "w",
        driver="GTiff",
        width=320,
        height=240,
        count=2,
        dtype="float64",
        nodata=9999.0,
        crs="EPSG:4326",
        transform=Affine(0.05, 0, -100.0, 0, -0.05, 45.0),
    ) as dst:
        dst.write(np.stack([b1, b2]))
    return path


@pytest.mark.parametrize("resampling", ["nearest", "bilinear", "cubic"])
def test_band_subset_reads_match_rasterio(two_band_edges, resampling):
    """Reads of some of the bands are assembled from warped blocks, each warped with every band
    (a read of all of them is warped straight into its buffer, with another kernel scale). GDAL
    3.12 unifies the bands' validity block by block, so in a block where band 1 has no nodata
    pixel every pixel is valid, and band 2's nodata values there are warped (and moved off the
    nodata value)."""
    rs, rv, ts, tv = _pair(two_band_edges, crs="EPSG:3857", resampling=resampling)
    with rs, rv, ts, tv:
        h, w = rv.height, rv.width
        reads = []
        for vrt, window in ((rv, RW), (tv, TW)):
            reads.append(
                [
                    vrt.read(2),
                    vrt.read([2, 1]),
                    vrt.read(2, window=window(10, 120, 150, 60)),
                    vrt.read([1], window=window(10, 120, 150, 60)),
                    vrt.read([2], out_shape=(1, h // 3, w // 3)),
                ]
            )
    for i, (a, b) in enumerate(zip(*reads, strict=True)):
        assert a.shape == b.shape and a.dtype == b.dtype, i
        assert np.array_equal(a, b), f"read {i}: {np.count_nonzero(a != b)} of {a.size} differ"


def _same_nodata(a, b):
    """Whether two nodata values are the same (NaN the same as NaN)."""
    if a is None or b is None:
        return a is None and b is None
    return a == b or (math.isnan(a) and math.isnan(b))


def _reads(vrt, window):
    """The reads compared: whole, a window (warped blocks), decimated, and the masks."""
    h, w = vrt.height, vrt.width
    return [
        vrt.read(1),
        vrt.read(1, window=window(w // 5, h // 4, w // 2, h // 3)),
        vrt.read(1, out_shape=(h // 3, w // 3)),
        vrt.read_masks(1),
    ]


@pytest.fixture(scope="module")
def rotated_sources(tmp_path_factory):
    """One-band WGS 84 GeoTIFFs of each type on a rotated grid, so that the grid GDAL suggests
    has corners outside the source; no nodata. A 40 x 40 block holds 7 (-7 in the signed types).
    (Through UTM, the warper's source coordinates differ from GDAL's by up to 5e-11 px, which
    shows in bilinear Float32 and Float64 values: see `test_float32_reads_match_rasterio`.)"""
    d = tmp_path_factory.mktemp("rotated")
    rng = np.random.default_rng(4)
    paths = {}
    for dtype in ("uint8", "int8", "uint16", "int16", "uint32", "int32", "float32"):
        if dtype == "float32":
            a = rng.normal(0, 50, (120, 120)).astype(dtype)
        elif dtype.startswith("u"):
            a = rng.integers(0, 120, (120, 120)).astype(dtype)
        else:
            a = rng.integers(-60, 60, (120, 120)).astype(dtype)
        a[30:70, 40:80] = 7 if dtype.startswith("u") else -7
        paths[dtype] = str(d / f"{dtype}.tif")
        with rasterio.open(
            paths[dtype],
            "w",
            driver="GTiff",
            width=120,
            height=120,
            count=1,
            dtype=dtype,
            crs="EPSG:4326",
            transform=Affine(0.024, 0.012, 10.0, 0.012, -0.024, 50.0),
        ) as dst:
            dst.write(a[np.newaxis])
    return paths


# The type of a warped VRT without `dtype`, as rasterio 1.5.0 (GDAL 3.12.1) gives it: the source's
# widened to hold the nodata value (`GDALWarpResolveWorkingDataType()`, from Byte: an Int8 source
# gives Int16), for `nodata=` and for `src_nodata=` alone; `src_nodata=` with `nodata=None` keeps
# the source's type when it cannot hold the value (GDAL drops it).
WIDENED = [
    ("uint8", -1, "int16", "uint8"),
    ("uint8", 300, "uint16", "uint8"),
    ("uint8", -2.5, "float32", "uint8"),
    ("uint8", float("nan"), "float64", "uint8"),
    ("int8", -7, "int16", "int16"),
    ("uint16", -1, "int32", "uint16"),
    ("int16", 40000, "int32", "int16"),
    ("uint32", -1, "int64", "uint32"),
    ("int32", 3e9, "int64", "int32"),
    ("float32", 0.1, "float64", "float32"),
    ("float32", 1e40, "float64", "float32"),
    ("float32", float("nan"), "float32", "float32"),
]


@pytest.mark.parametrize("resampling", ["nearest", "bilinear"])
@pytest.mark.parametrize("given", ["nodata", "src_nodata", "src_nodata only"])
@pytest.mark.parametrize("dtype,value,widened,kept", WIDENED, ids=lambda v: str(v))
def test_nodata_widens_the_type_as_rasterio(
    rotated_sources, dtype, value, widened, kept, given, resampling
):
    """A nodata value the source's type cannot hold: the VRT's type holds it, the corners outside
    the source hold it, and they are masked, as rasterio has them; but Int64 and UInt64 bands,
    whose nodata value rasterio sets as a double, mask nothing. rasterio reports the nodata value
    it was given."""
    kw = {"nodata": value} if given == "nodata" else {"src_nodata": value}
    if given == "src_nodata only":
        kw["nodata"] = None
    rs, rv, ts, tv = _pair(rotated_sources[dtype], crs="EPSG:4326", resampling=resampling, **kw)
    with rs, rv, ts, tv:
        assert tv.dtypes == rv.dtypes == ((kept if given == "src_nodata only" else widened),)
        assert _same_nodata(tv.nodata, rv.nodata) and len(tv.nodatavals) == len(rv.nodatavals)
        assert all(_same_nodata(a, b) for a, b in zip(tv.nodatavals, rv.nodatavals, strict=True))
        flags = [[[f.value for f in band] for band in v.mask_flag_enums] for v in (rv, tv)]
        assert flags[0] == flags[1]
        reads = [_reads(rv, RW), _reads(tv, TW)]
    for i, (a, b) in enumerate(zip(*reads, strict=True)):
        assert a.shape == b.shape and a.dtype == b.dtype, i
        same = (a == b) | (np.isnan(a) & np.isnan(b)) if a.dtype.kind == "f" else a == b
        assert same.all(), f"read {i}: {np.count_nonzero(~same)} of {a.size} differ"
    masked = np.count_nonzero(reads[1][3] == 0)
    if given == "src_nodata only" or widened in ("int64", "uint64"):
        assert masked == 0
    else:
        assert masked > 0


def test_byte_source_with_nodata_minus_one_is_masked_as_rasterio(tmp_path):
    """A Byte source warped with `nodata=-1` (and no `dtype`): an Int16 VRT whose corners
    outside the source are -1 and masked, as rasterio has them, rather than Byte zeros."""
    path = str(tmp_path / "byte_far.tif")
    rng = np.random.default_rng(4)
    with rasterio.open(
        path,
        "w",
        driver="GTiff",
        width=400,
        height=400,
        count=1,
        dtype="uint8",
        crs="EPSG:32633",
        transform=Affine(500, 0, 150000, 0, -500, 7000000),
    ) as dst:
        dst.write(rng.integers(1, 255, (1, 400, 400), dtype="uint8"))
    rs, rv, ts, tv = _pair(path, crs="EPSG:4326", nodata=-1)
    with rs, rv, ts, tv:
        assert (tv.dtypes, tv.nodata) == (rv.dtypes, rv.nodata) == (("int16",), -1)
        a, m = rv.read(1), rv.read_masks(1)
        b, n = tv.read(1), tv.read_masks(1)
    assert np.array_equal(a, b) and np.array_equal(m, n)
    assert np.count_nonzero(n == 0) == np.count_nonzero(b == -1) > 0


@pytest.mark.parametrize("dtype,value", [("uint8", -1), ("uint8", 300), ("int16", -1)])
def test_given_dtype_keeps_the_nodata_value_rasterio_reports(rotated_sources, dtype, value):
    """With `dtype`, the type is the given one: rasterio still reports the nodata value it set,
    and a band masks by it only when its type's range holds it."""
    rs, rv, ts, tv = _pair(rotated_sources["uint8"], crs="EPSG:4326", nodata=value, dtype=dtype)
    with rs, rv, ts, tv:
        assert tv.dtypes == rv.dtypes == (dtype,)
        assert tv.nodata == rv.nodata == value
        reads = [_reads(rv, RW), _reads(tv, TW)]
    for i, (a, b) in enumerate(zip(*reads, strict=True)):
        assert a.dtype == b.dtype and np.array_equal(a, b), i
    assert (np.count_nonzero(reads[1][3] == 0) > 0) == (dtype == "int16")


def test_source_nodata_none_warps_with_the_bands_nodata_as_rasterio(tmp_path):
    """`src_nodata=None` on a source with a nodata value, on the grid GDAL suggests:
    `GDALAutoCreateWarpedVRTEx()` gives the warp the band's own nodata value, so its pixels are
    not warped (the VRT has no nodata value, and they read 0)."""
    path = str(tmp_path / "nodata7.tif")
    a = np.random.default_rng(2).integers(1, 120, (120, 120)).astype("uint8")
    a[30:70, 40:80] = 7
    with rasterio.open(
        path,
        "w",
        driver="GTiff",
        width=120,
        height=120,
        count=1,
        dtype="uint8",
        nodata=7,
        crs="EPSG:32633",
        transform=Affine(1500, 0, 150000, 0, -1500, 7000000),
    ) as dst:
        dst.write(a[np.newaxis])
    for resampling in ("nearest", "bilinear"):
        rs, rv, ts, tv = _pair(path, crs="EPSG:4326", src_nodata=None, resampling=resampling)
        with rs, rv, ts, tv:
            assert tv.nodata is None and rv.nodata is None
            reads = [_reads(rv, RW), _reads(tv, TW)]
        for i, (x, y) in enumerate(zip(*reads, strict=True)):
            assert x.dtype == y.dtype and np.array_equal(x, y), (resampling, i)
        assert np.count_nonzero(reads[1][0] == 7) == 0
