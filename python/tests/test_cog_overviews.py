"""COG overviews against GDAL's COG driver: the same levels and the same pixels.

An ingestion pipeline converts uploads with `rasterio.shutil.copy(src, dst, driver="COG", ...)`,
whose overviews GDAL resamples with CUBIC unless told otherwise. Each case writes a source with
GDAL (rasterio), converts it with GDAL and with terrio, and compares every level of every band,
pixel for pixel: overview count, sizes and values. Skipped without rasterio.

Tolerance: none. terrio follows GDAL 3.12.1's overview code function by function
(`gcore/overview.cpp`), so any difference fails.
"""

import numpy as np
import pytest

import terrio
import terrio.rio as trio
import terrio.rio.shutil  # noqa: F401

rasterio = pytest.importorskip("rasterio")
import rasterio.shutil  # noqa: E402
from rasterio.enums import Resampling as RR  # noqa: E402
from rasterio.transform import from_origin  # noqa: E402

from terrio.rio.enums import Resampling as TR  # noqa: E402

METHODS = [
    "NEAREST",
    "AVERAGE",
    "RMS",
    "BILINEAR",
    "CUBIC",
    "CUBICSPLINE",
    "LANCZOS",
    "MODE",
    "GAUSS",
]

# (width, height, bands, dtype, nodata): odd and even sizes, 1 and 3 bands, with and without
# nodata. 875 halves to 437, as the 875 x 875 level of a 14000 x 14000 raster does.
CASES = [
    (1000, 1000, 1, "uint8", None),
    (1001, 999, 3, "uint8", 0),
    (1001, 999, 3, "uint16", None),
    (3001, 2999, 1, "uint16", 0),
    (1001, 999, 1, "int16", -9999),
    (875, 875, 3, "int16", None),
    (1001, 999, 1, "float32", None),
    (875, 875, 3, "float32", -9999.0),
]

RANGES = {"uint8": (0, 255), "uint16": (0, 10000), "int16": (-3000, 3000), "float32": (-50.0, 50.0)}


def _source(path, width, height, bands, dtype, nodata, **profile):
    """A smooth field plus noise, with scattered nodata pixels and a nodata block."""
    rng = np.random.default_rng(width * 7 + height + bands)
    y, x = np.mgrid[0:height, 0:width].astype("float64")
    lo, hi = RANGES[dtype]
    planes = []
    for b in range(bands):
        base = (np.sin(x / (17 + 5 * b)) + np.cos(y / (23 + 3 * b))) * 0.25 + 0.5
        planes.append(lo + (hi - lo) * np.clip(base + rng.normal(0, 0.05, base.shape), 0, 1))
    data = np.stack(planes).astype(dtype)
    if nodata is not None:
        holes = rng.random((height, width)) < 0.02
        holes[: height // 3, : width // 4] = True
        data[:, holes] = nodata
    with rasterio.open(
        path,
        "w",
        driver="GTiff",
        width=width,
        height=height,
        count=bands,
        dtype=dtype,
        nodata=nodata,
        crs="EPSG:32613",
        transform=from_origin(400000, 4500000, 10, 10),
        **profile,
    ) as dst:
        dst.write(data)
    return str(path)


def _levels(path):
    """Every level (full resolution, then each overview) of a file, read by GDAL."""
    with rasterio.open(path) as ds:
        count = len(ds.overviews(1))
        out = [ds.read()]
    for i in range(count):
        with rasterio.open(path, overview_level=i) as ds:
            out.append(ds.read())
    return out


def _assert_same(expected, actual, what):
    a, b = _levels(expected), _levels(actual)
    assert [x.shape for x in b] == [x.shape for x in a], f"{what}: levels"
    for i, (x, y) in enumerate(zip(a, b)):
        same = (x == y) | (np.isnan(x) & np.isnan(y)) if x.dtype.kind == "f" else x == y
        if not same.all():
            d = np.abs(x.astype("float64") - y.astype("float64"))[~same]
            pytest.fail(
                f"{what}: level {i} {x.shape}: {np.mean(~same):.4%} of pixels differ, "
                f"max |d| {np.nanmax(d) if d.size else 'nan'}"
            )


def _gdal_cog(src, dst, **options):
    rasterio.shutil.copy(src, dst, driver="COG", compress="deflate", **options)
    return str(dst)


@pytest.mark.parametrize("method", METHODS)
@pytest.mark.parametrize(
    "case", CASES, ids=lambda c: "x".join(map(str, c[:3])) + f"-{c[3]}-nd{c[4]}"
)
def test_overviews_are_gdals(tmp_path, case, method):
    """Each resampling, through `terrio.convert` (the native path) and `OVERVIEW_RESAMPLING`."""
    src = _source(tmp_path / "src.tif", *case)
    gdal = _gdal_cog(src, tmp_path / "gdal.tif", overview_resampling=method)
    ours = str(tmp_path / "terrio.tif")
    terrio.convert(
        src, ours, format="COG", compress="DEFLATE", options={"OVERVIEW_RESAMPLING": method.lower()}
    )
    _assert_same(gdal, ours, f"convert {method}")


@pytest.mark.parametrize("path", ["convert", "rio_copy", "rio_open", "raster_write"])
def test_default_resampling_is_gdals_cubic(tmp_path, path):
    """No resampling asked for, as in ingestion: GDAL's default (CUBIC), on every path."""
    src = _source(tmp_path / "src.tif", 1001, 999, 3, "uint16", 0)
    gdal = _gdal_cog(src, tmp_path / "gdal.tif")
    ours = str(tmp_path / "terrio.tif")
    if path == "convert":
        terrio.convert(src, ours, format="COG", compress="DEFLATE")
    elif path == "rio_copy":
        with trio.open(src) as ds:
            profile = ds.profile.copy()
        profile.update(driver="COG", compress="deflate")
        trio.shutil.copy(src, ours, **profile)
    elif path == "rio_open":
        with trio.open(src) as ds:
            profile, data = ds.profile.copy(), ds.read()
        profile.update(driver="COG", compress="deflate")
        with trio.open(ours, "w", **profile) as dst:
            dst.write(data)
    else:
        terrio.open(src).write(ours, driver="COG", compress="DEFLATE")
    _assert_same(gdal, ours, path)


@pytest.mark.parametrize("key", ["overview_resampling", "resampling"])
def test_resampling_keywords_on_rio_paths(tmp_path, key):
    """rasterio's `overview_resampling` and GDAL's `RESAMPLING` creation option."""
    src = _source(tmp_path / "src.tif", 875, 875, 1, "float32", -9999.0)
    gdal = _gdal_cog(src, tmp_path / "gdal.tif", **{key: "lanczos"})
    ours = str(tmp_path / "terrio.tif")
    trio.shutil.copy(src, ours, driver="COG", compress="deflate", **{key: "lanczos"})
    _assert_same(gdal, ours, key)
    written = str(tmp_path / "written.tif")
    terrio.open(src).write(
        written, driver="COG", compress="DEFLATE", options={key.upper(): "LANCZOS"}
    )
    _assert_same(gdal, written, f"Raster.write {key}")


def test_paletted_default_is_nearest(tmp_path):
    """GDAL's COG driver resamples paletted rasters with NEAREST by default (terrio.rio keeps
    the color table; `terrio.convert` does not carry one)."""
    src = str(tmp_path / "src.tif")
    rng = np.random.default_rng(1)
    with rasterio.open(
        src, "w", driver="GTiff", width=1001, height=999, count=1, dtype="uint8"
    ) as dst:
        dst.write(rng.integers(0, 8, (1, 999, 1001), dtype="uint8"))
        dst.write_colormap(1, {i: (i * 30, 255 - i * 30, i * 10, 255) for i in range(8)})
    gdal = _gdal_cog(src, tmp_path / "gdal.tif")
    ours = str(tmp_path / "terrio.tif")
    trio.shutil.copy(src, ours, driver="COG", compress="deflate")
    _assert_same(gdal, ours, "paletted")


def test_overview_count_and_tiled_source(tmp_path):
    """`OVERVIEW_COUNT`, and a tiled source, whose blocks set those GDAL computes overviews in."""
    src = _source(
        tmp_path / "src.tif",
        3001,
        2999,
        3,
        "uint8",
        None,
        tiled=True,
        blockxsize=256,
        blockysize=256,
    )
    for options in (
        {"overview_resampling": "bilinear"},
        {"overview_resampling": "cubic", "overview_count": 2},
    ):
        gdal = _gdal_cog(src, tmp_path / "gdal.tif", **options)
        ours = str(tmp_path / "terrio.tif")
        terrio.convert(
            src,
            ours,
            format="COG",
            compress="DEFLATE",
            options={k.upper(): str(v) for k, v in options.items()},
        )
        _assert_same(gdal, ours, f"tiled source {options}")


# Overviews of rasters with an alpha band or a per-dataset mask. GDAL weights each level's
# kernel by the mask of the level it is computed from (`GDALRegenerateOverviewsMultiBand`): an
# alpha band by itself, the other bands by the alpha band (of two- or four-band rasters), by the
# internal mask, or by their nodata mask, which comes before the alpha band. The internal mask's
# own overviews come first, each weighted by itself.

# The resamplings an ingestion pipeline uses (None: the COG driver's default, as it asks for none).
MASK_METHODS = [None, "NEAREST", "AVERAGE", "BILINEAR", "CUBIC", "MODE", "GAUSS"]

# kind: (bands, dtype, nodata, alpha, internal mask).
MASKED = {
    "rgba": (4, "uint8", None, True, False),
    "rgba-nodata": (4, "uint8", 0, True, False),
    "mask-rgb": (3, "uint8", None, False, True),
    "mask-u16-nodata": (1, "uint16", 0, False, True),
    "mask-f32": (1, "float32", None, False, True),
}


def _masked_source(path, kind, width, height, **profile):
    """A smooth field; an alpha band with a transparent block, soft edges (partial alpha) and
    holes, or the same as an internal mask (0 or 255)."""
    bands, dtype, nodata, alpha, mask = MASKED[kind]
    rng = np.random.default_rng(width + height + bands)
    y, x = np.mgrid[0:height, 0:width].astype("float64")
    lo, hi = RANGES[dtype]
    colors = bands - 1 if alpha else bands
    planes = [
        lo
        + (hi - lo)
        * np.clip(
            np.sin(x / (15 + 4 * b)) * 0.3
            + np.cos(y / 21) * 0.2
            + 0.5
            + rng.normal(0, 0.05, x.shape),
            0,
            1,
        )
        for b in range(colors)
    ]
    a = np.full((height, width), 255.0)
    a[: height // 3, : width // 4] = 0
    edge = (x > width // 2) & (x < width // 2 + 40)
    a[edge] = (x[edge] - width // 2) * 6
    a[rng.random(a.shape) < 0.02] = 0
    data = np.stack(planes).astype(dtype)
    if nodata is not None:
        data[:, rng.random((height, width)) < 0.02] = nodata
    if alpha:
        data = np.concatenate([data, a[None].astype(dtype)])
    kw = {"photometric": "RGB", "alpha": "YES"} if alpha else {}
    with rasterio.Env(GDAL_TIFF_INTERNAL_MASK=True):
        with rasterio.open(
            path,
            "w",
            driver="GTiff",
            width=width,
            height=height,
            count=bands,
            dtype=dtype,
            nodata=nodata,
            crs="EPSG:32613",
            transform=from_origin(400000, 4500000, 10, 10),
            **kw,
            **profile,
        ) as dst:
            dst.write(data)
            if mask:
                dst.write_mask(np.where(a > 0, 255, 0).astype("uint8"))
    return str(path)


def _levels_and_masks(path):
    """Every level's pixels and masks (`read_masks`: the internal mask's overviews, or the alpha
    band's), read by GDAL."""
    out = []
    with rasterio.open(path) as ds:
        count = len(ds.overviews(1))
        out.append((ds.read(), ds.read_masks()))
    for i in range(count):
        with rasterio.open(path, overview_level=i) as ds:
            out.append((ds.read(), ds.read_masks()))
    return out


def _assert_same_masked(expected, actual, what):
    a, b = _levels_and_masks(expected), _levels_and_masks(actual)
    assert [x[0].shape for x in b] == [x[0].shape for x in a], f"{what}: levels"
    for i, ((x, xm), (y, ym)) in enumerate(zip(a, b)):
        diff = np.mean(xm != ym)
        assert diff == 0, f"{what}: level {i} {x.shape}: {diff:.4%} of the mask differs"
        same = (x == y) | (np.isnan(x) & np.isnan(y)) if x.dtype.kind == "f" else x == y
        if not same.all():
            d = np.abs(x.astype("float64") - y.astype("float64"))[~same]
            pytest.fail(
                f"{what}: level {i} {x.shape}: {np.mean(~same):.4%} of pixels differ, "
                f"max |d| {np.nanmax(d) if d.size else 'nan'}"
            )


@pytest.mark.parametrize("method", MASK_METHODS, ids=lambda m: m or "default")
@pytest.mark.parametrize("kind", list(MASKED))
def test_masked_overviews_are_gdals(tmp_path, kind, method):
    """An ingestion pipeline's call (the source's profile, `driver="COG"`, `compress="deflate"`
    through `shutil.copy`) on rasters with an alpha band or an internal mask: every level's pixels
    and masks, over three levels (cascaded from the mask overviews)."""
    src = _masked_source(tmp_path / "src.tif", kind, 2100, 1900)
    extra = {} if method is None else {"overview_resampling": method}
    with rasterio.open(src) as ds:
        profile = ds.profile | {"driver": "COG", "compress": "deflate"} | extra
    gdal = str(tmp_path / "gdal.tif")
    rasterio.shutil.copy(src, gdal, **profile)
    ours = str(tmp_path / "terrio.tif")
    trio.shutil.copy(src, ours, **profile)
    _assert_same_masked(gdal, ours, f"{kind} {method}")


@pytest.mark.parametrize("size", [(1001, 999), (2100, 1900)], ids=["2-levels", "3-levels"])
@pytest.mark.parametrize("kind", ["rgba", "mask-rgb", "mask-u16-nodata", "mask-f32"])
def test_masked_overviews_on_rio_open(tmp_path, kind, size):
    """`rio.open(..., "w", driver="COG")`, with the alpha band among the bands or `write_mask`.

    rasterio buffers such a write in a MEM dataset, whose mask band GDAL creates without a
    dataset (`MEMRasterBand::CreateMaskBand`): the temporary mask overviews then lack
    INTERNAL_MASK_FLAGS_1, so the mask's overviews after the first are weighted by an all-valid
    mask rather than by their own values, as a file's mask is (the shutil.copy path above), and
    the data's by those Byte overviews."""
    src = _masked_source(tmp_path / "src.tif", kind, *size)
    gdal, ours = str(tmp_path / "gdal.tif"), str(tmp_path / "terrio.tif")
    for mod, out in ((rasterio, gdal), (trio, ours)):
        with mod.open(src) as ds:
            profile = ds.profile | {"driver": "COG", "compress": "deflate", "blocksize": 256}
            data, mask, colorinterp = ds.read(), ds.dataset_mask(), ds.colorinterp
        with mod.open(out, "w", **profile) as dst:
            dst.colorinterp = colorinterp
            dst.write(data)
            if MASKED[kind][4]:
                dst.write_mask(mask)
    _assert_same_masked(gdal, ours, f"rio.open {kind}")


_BUILD_METHODS = [m for m in MASK_METHODS if m]


@pytest.mark.parametrize("method", _BUILD_METHODS)
@pytest.mark.parametrize("kind", ["rgba", "mask-rgb", "mask-f32"])
def test_masked_build_overviews_are_gdals(tmp_path, kind, method):
    """`build_overviews` on a GeoTIFF with an alpha band or an internal mask, whose mask
    overviews GDAL builds first (with nearest neighbour, each from the full resolution)."""
    src = _masked_source(tmp_path / "src.tif", kind, 1001, 999)
    gdal, ours = str(tmp_path / "gdal.tif"), str(tmp_path / "terrio.tif")
    for mod, out, enums in ((rasterio, gdal, RR), (trio, ours, TR)):
        with mod.open(src) as ds:
            profile = ds.profile | {
                "tiled": True,
                "blockxsize": 256,
                "blockysize": 256,
                "compress": "deflate",
            }
            data, mask = ds.read(), ds.dataset_mask()
            colorinterp = ds.colorinterp
        with mod.open(out, "w", **profile) as dst:
            dst.colorinterp = colorinterp
            dst.write(data)
            if MASKED[kind][4]:
                dst.write_mask(mask)
            dst.build_overviews([2, 4, 8], enums[method.lower()])
    _assert_same_masked(gdal, ours, f"build_overviews {kind} {method}")


@pytest.mark.parametrize("kind", ["ycbcr-jpeg", "palette", "gray-resampling-given"])
def test_ingestion_call_structure_is_gdals(tmp_path, kind):
    """The ingestion call's photometric interpretation and IMAGE_STRUCTURE metadata: GDAL's COG
    driver has no PHOTOMETRIC option (a YCbCr source's profile asks for one), and records the
    overviews' resampling (`OVERVIEW_RESAMPLING`: as given, else CUBIC, NEAREST if paletted)."""
    src = str(tmp_path / "src.tif")
    rng = np.random.default_rng(3)
    y, x = np.mgrid[0:999, 0:1001]
    field = (np.sin(x / 17) * 60 + np.cos(y / 23) * 50 + 120 + rng.normal(0, 5, x.shape)).clip(
        0, 255
    )
    profile = dict(
        driver="GTiff",
        width=1001,
        height=999,
        crs="EPSG:32613",
        transform=from_origin(400000, 4500000, 10, 10),
    )
    extra = {}
    if kind == "ycbcr-jpeg":
        profile.update(count=3, dtype="uint8", photometric="YCBCR", compress="jpeg", tiled=True)
        data = np.stack([field, field[::-1], field[:, ::-1]]).astype("uint8")
    elif kind == "palette":
        profile.update(count=1, dtype="uint8")
        data = (field[None] // 26).astype("uint8")
    else:
        profile.update(count=1, dtype="uint16")
        data = (field[None] * 200).astype("uint16")
        extra = {"overview_resampling": "average"}
    with rasterio.open(src, "w", **profile) as dst:
        dst.write(data)
        if kind == "palette":
            dst.write_colormap(
                1, {c: (25 * c, 255 - 20 * c, (60 * c) % 256, 255) for c in range(10)}
            )
    outputs = {}
    for name, mod in (("gdal", rasterio), ("terrio", trio)):
        out = str(tmp_path / f"{name}.tif")
        with mod.open(src) as ds:
            p = ds.profile.copy()
        p.update({"driver": "COG", "compress": "deflate"}, **extra)
        mod.shutil.copy(src, out, **p)
        with rasterio.open(out) as ds:
            outputs[name] = (
                ds.profile.get("photometric"),
                ds.colorinterp,
                ds.tags(ns="IMAGE_STRUCTURE"),
            )
        if kind != "palette":
            continue
        with rasterio.open(out) as ds:
            outputs[name] += (ds.colormap(1),)
    assert outputs["terrio"] == outputs["gdal"]
    _assert_same_masked(str(tmp_path / "gdal.tif"), str(tmp_path / "terrio.tif"), kind)


@pytest.mark.parametrize("kind", ["u16-average", "rgb-nearest", "rgb-mask", "rgb-mask-after"])
def test_ingestion_call_copies_source_overviews(tmp_path, kind):
    """A source with overviews of its own: GDAL's COG driver copies them (OVERVIEWS=AUTO), with
    their levels, values, masks (or makes the mask's when it has none) and `RESAMPLING`."""
    src = str(tmp_path / "src.tif")
    if kind == "u16-average":
        _source(src, 1001, 999, 1, "uint16", 0, tiled=True, blockxsize=256, blockysize=256)
    else:
        _masked_source(src, "mask-rgb", 1001, 999, tiled=True, blockxsize=256, blockysize=256)
    with rasterio.open(src, "r+") as ds:
        mask = ds.dataset_mask()
        ds.build_overviews([2, 4, 8], RR.nearest if kind == "rgb-nearest" else RR.average)
    if kind == "rgb-mask-after":
        # Rewritten with the overviews first and the mask after, which then has none.
        with rasterio.open(src) as ds:
            profile, data = ds.profile, ds.read()
        with rasterio.open(src, "w", **profile) as dst:
            dst.write(data)
            dst.build_overviews([2, 4, 8], RR.average)
            dst.write_mask(mask)
    for name, mod in (("gdal", rasterio), ("terrio", trio)):
        with mod.open(src) as ds:
            p = ds.profile.copy()
        p.update({"driver": "COG", "compress": "deflate"})
        mod.shutil.copy(src, str(tmp_path / f"{name}.tif"), **p)
    gdal, ours = str(tmp_path / "gdal.tif"), str(tmp_path / "terrio.tif")
    with rasterio.open(gdal) as g, rasterio.open(ours) as t:
        assert t.overviews(1) == g.overviews(1) == [2, 4, 8]
        assert t.tags(ns="IMAGE_STRUCTURE") == g.tags(ns="IMAGE_STRUCTURE")
    _assert_same_masked(gdal, ours, kind)


def test_cog_writes_stream(tmp_path, monkeypatch):
    """terrio.rio writes COGs without copying the whole raster into Rust (`Raster.from_array`),
    level rows computed as they come: a memory property no file shows (the files are GDAL's)."""
    src = _source(tmp_path / "src.tif", 1001, 999, 3, "uint16", 0)
    gdal = _gdal_cog(src, tmp_path / "gdal.tif")
    with trio.open(src) as ds:
        profile = ds.profile | {"driver": "COG", "compress": "deflate"}
        data = ds.read()

    def whole(*args, **kwargs):
        raise AssertionError("the COG write copied the whole raster")

    monkeypatch.setattr(terrio._terrio.Raster, "from_array", whole)
    trio.shutil.copy(src, str(tmp_path / "copy.tif"), **profile)
    with trio.open(str(tmp_path / "open.tif"), "w", **profile) as dst:
        dst.write(data)
    monkeypatch.undo()
    _assert_same(gdal, str(tmp_path / "copy.tif"), "shutil.copy")
    _assert_same(gdal, str(tmp_path / "open.tif"), "rio.open")


def _cog_view(path):
    """A COG as GDAL reads it: structure, nodata, descriptions and each level's pixels and masks."""
    with rasterio.open(path) as ds:
        keys = ("dtype", "count", "interleave", "blockxsize", "blockysize", "compress", "tiled")
        view = {
            "image_structure": ds.tags(ns="IMAGE_STRUCTURE"),
            "profile": {k: ds.profile.get(k) for k in keys},
            "nodata": repr(ds.nodatavals),
            "descriptions": ds.descriptions,
            "overviews": ds.overviews(1),
        }
        levels = [(ds.read(), ds.read_masks())]
    for i in range(len(view["overviews"])):
        with rasterio.open(path, overview_level=i) as ds:
            levels.append((ds.read(), ds.read_masks()))
    return view, levels


def _assert_same_cog(gdal, ours):
    (gv, gl), (tv, tl) = _cog_view(gdal), _cog_view(ours)
    assert tv == gv
    for (ga, gm), (ta, tm) in zip(gl, tl, strict=True):
        assert np.array_equal(ta, ga, equal_nan=True) and np.array_equal(tm, gm)


def test_multiband_float32_image_chip(tmp_path):
    """A statistics service's image chip: a multi-band float32 COG, NaN nodata, DEFLATE and band
    descriptions through MemoryFile.open(driver="COG"), as GDAL writes it."""
    import rasterio.io
    import terrio.rio.io

    values = np.random.default_rng(3).random((3, 900, 1100), dtype="float32")
    values[:, :40, :50] = np.nan
    values[1, 500:, 700:] = np.nan
    for name, memory_file in (
        ("gdal", rasterio.io.MemoryFile),
        ("terrio", terrio.rio.io.MemoryFile),
    ):
        with memory_file() as memory:
            with memory.open(
                driver="COG",
                height=900,
                width=1100,
                count=3,
                dtype="float32",
                crs="EPSG:32633",
                transform=from_origin(5e5, 4.1e6, 10.0, 10.0),
                nodata=np.nan,
                compress="DEFLATE",
            ) as dataset:
                dataset.write(values)
                for band_index, band in enumerate(("ndvi", "evi", "ndwi"), start=1):
                    dataset.set_band_description(band_index, band)
            (tmp_path / f"{name}.tif").write_bytes(memory.read())
    _assert_same_cog(str(tmp_path / "gdal.tif"), str(tmp_path / "terrio.tif"))


@pytest.mark.parametrize("memory", [False, True], ids=["open", "memoryfile"])
@pytest.mark.parametrize(
    "options",
    [{}, {"compress": "NONE"}, {"compress": "DEFLATE"}, {"interleave": "pixel"}],
    ids=["default", "none", "deflate", "pixel"],
)
@pytest.mark.parametrize("count", [1, 3])
def test_cog_open_interleave(tmp_path, count, options, memory):
    """rasterio writes a COG by copying a MEM dataset, band-interleaved: GDALDriver::CreateCopy
    keeps BAND unless INTERLEAVE or a COMPRESS other than NONE is given."""
    import rasterio.io
    import terrio.rio.io

    data = (np.arange(count * 700 * 600) % 251).astype("uint16").reshape(count, 700, 600)
    kw = dict(
        driver="COG",
        height=700,
        width=600,
        count=count,
        dtype="uint16",
        crs="EPSG:32633",
        transform=from_origin(0, 700, 1, 1),
        **options,
    )
    for name, mod, memory_file in (
        ("gdal", rasterio, rasterio.io.MemoryFile),
        ("terrio", trio, terrio.rio.io.MemoryFile),
    ):
        path = tmp_path / f"{name}.tif"
        if memory:
            with memory_file() as mf:
                with mf.open(**kw) as ds:
                    ds.write(data)
                path.write_bytes(mf.read())
        else:
            with mod.open(path, "w", **kw) as ds:
                ds.write(data)
    _assert_same_cog(str(tmp_path / "gdal.tif"), str(tmp_path / "terrio.tif"))


@pytest.mark.parametrize("options", [{}, {"compress": "deflate"}], ids=["default", "deflate"])
@pytest.mark.parametrize("src_interleave", ["pixel", "band"])
@pytest.mark.parametrize("driver", ["COG", "GTiff"])
def test_copy_keeps_source_interleave(tmp_path, driver, src_interleave, options):
    """`shutil.copy` is GDAL's CreateCopy: the source's interleaving unless INTERLEAVE or a
    COMPRESS other than NONE is given, else the drivers' PIXEL."""
    src = str(tmp_path / "src.tif")
    with rasterio.open(
        src,
        "w",
        driver="GTiff",
        height=300,
        width=400,
        count=3,
        dtype="uint16",
        crs="EPSG:32633",
        transform=from_origin(0, 300, 1, 1),
        interleave=src_interleave,
    ) as ds:
        ds.write((np.arange(3 * 300 * 400) % 999).astype("uint16").reshape(3, 300, 400))
    for name, mod in (("gdal", rasterio), ("terrio", trio)):
        mod.shutil.copy(src, str(tmp_path / f"{name}.tif"), driver=driver, **options)
    with rasterio.open(tmp_path / "gdal.tif") as g, rasterio.open(tmp_path / "terrio.tif") as t:
        assert t.tags(ns="IMAGE_STRUCTURE") == g.tags(ns="IMAGE_STRUCTURE")
        assert np.array_equal(t.read(), g.read())
