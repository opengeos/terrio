"""Metadata domains in GeoTIFFs and COGs written by terrio.rio, against GDAL (rasterio).

GDAL's GTiff driver writes every metadata domain of a dataset it creates into `GDAL_METADATA`
(`GTiffDataset::WriteMetadata`, `WriteMDMetadata`: all but IMAGE_STRUCTURE, DERIVED_SUBDATASETS,
COLOR_PROFILE, RPC and xml:XMP). Its COG driver copies a dataset (`CreateCopy`), which keeps the
other domains only when `COPY_SRC_MDD` or `SRC_MDD` ask for them. An imagery pipeline records
its overviews' resampling in rasterio's `rio_overview` domain. Each case makes the same calls on
GDAL and on terrio.rio and compares what GDAL reads back, and the `GDAL_METADATA` items in
order. Skipped without rasterio.
"""

import shutil
import struct
import xml.etree.ElementTree as ET

import numpy as np
import pytest

import terrio.rio as trio
from terrio.rio.io import MemoryFile as TMemoryFile

rasterio = pytest.importorskip("rasterio")
from rasterio.enums import Resampling  # noqa: E402
from rasterio.io import MemoryFile  # noqa: E402
from rasterio.transform import from_origin  # noqa: E402

H, W = 300, 400
TRANSFORM = from_origin(10.0, 50.0, 0.001, 0.001)
DATA = (np.arange(2 * H * W) % 251).astype("uint8").reshape(2, H, W)


def gdal_metadata_items(path):
    """IFD 0's `GDAL_METADATA` items in order: (attributes, text)."""
    b = open(path, "rb").read()
    e = "<" if b[:2] == b"II" else ">"
    if struct.unpack(e + "H", b[2:4])[0] == 43:
        off = struct.unpack(e + "Q", b[8:16])[0]
        n, p, size, fmt, inline = struct.unpack(e + "Q", b[off : off + 8])[0], off + 8, 20, "HHQQ", 8
    else:
        off = struct.unpack(e + "I", b[4:8])[0]
        n, p, size, fmt, inline = struct.unpack(e + "H", b[off : off + 2])[0], off + 2, 12, "HHII", 4
    for i in range(n):
        entry = p + i * size
        tag, _, count, val = struct.unpack(e + fmt, b[entry : entry + size])
        if tag == 42112:
            start = entry + size - inline if count <= inline else val
            xml = b[start : start + count].rstrip(b"\0").decode()
            return [(dict(i.attrib), i.text) for i in ET.fromstring(xml).iter("Item")]
    return []


def gdal_view(path):
    """What GDAL reads: every domain of the dataset and of each band, and the raw items."""
    with rasterio.open(path) as ds:
        skip = ("IMAGE_STRUCTURE", "DERIVED_SUBDATASETS")
        view = {0: {d: ds.tags(ns=d) for d in ds.tag_namespaces() if d not in skip}}
        view[0][""] = ds.tags()
        for b in ds.indexes:
            view[b] = {d: ds.tags(b, ns=d) for d in ds.tag_namespaces(b)}
            view[b][""] = ds.tags(b)
    return view, gdal_metadata_items(path)


def domains(ds):
    ds.update_tags(a="1")
    ds.update_tags(ns="rio_overview", resampling="average")
    ds.update_tags(ns="foo", x="1", y="<2 & 3>")
    ds.update_tags(1, ns="bar", b="3")
    ds.update_tags(2, c="4")
    ds.update_tags(2, ns="foo", z="5")
    ds.set_band_description(1, "first")


def write(mod, path, driver, **opts):
    profile = dict(height=H, width=W, count=2, dtype="uint8", crs="EPSG:4326", transform=TRANSFORM)
    with mod.open(path, "w", driver=driver, **profile, **opts) as ds:
        ds.write(DATA)
        domains(ds)


@pytest.mark.parametrize(
    "driver, opts, spill",
    [
        ("GTiff", {}, False),
        ("GTiff", {"tiled": True, "compress": "deflate"}, True),  # written in rows
        ("COG", {"compress": "deflate"}, False),  # written in rows: GDAL drops the domains
        ("COG", {"COPY_SRC_MDD": "YES"}, False),
        ("COG", {"SRC_MDD": "foo"}, False),
    ],
    ids=["gtiff", "gtiff-streamed", "cog", "cog-copy-src-mdd", "cog-src-mdd"],
)
def test_domains_written_as_gdal(tmp_path, monkeypatch, driver, opts, spill):
    """Each domain where GDAL writes it, or nowhere when GDAL drops it, in GDAL's item order."""
    if spill:
        monkeypatch.setenv("TERRIO_RIO_IN_MEMORY_LIMIT", "1000")
    write(rasterio, tmp_path / "gdal.tif", driver, **opts)
    write(trio, tmp_path / "terrio.tif", driver, **opts)
    assert gdal_view(tmp_path / "terrio.tif") == gdal_view(tmp_path / "gdal.tif")


def test_rio_overview_domain_is_kept(tmp_path):
    """An imagery pipeline's overview recording on both: the `rio_overview` domain is kept."""
    for mod, memory_file, name in ((rasterio, MemoryFile, "gdal"), (trio, TMemoryFile, "terrio")):
        with memory_file() as memfile:
            with memfile.open(driver="GTiff", height=H, width=W, count=2, dtype="uint8",
                              crs="EPSG:4326", transform=TRANSFORM, compress="deflate",
                              tiled=True, blockxsize=128, blockysize=128) as dst_ds:
                dst_ds.write(DATA)
                dst_ds.build_overviews([2, 4], Resampling.average)
                dst_ds.update_tags(ns="rio_overview", resampling="average")
            (tmp_path / f"{name}.tif").write_bytes(memfile.read())
    view = gdal_view(tmp_path / "gdal.tif")
    assert view[0][0]["rio_overview"] == {"resampling": "average"}
    assert gdal_view(tmp_path / "terrio.tif") == view
    with trio.open(tmp_path / "terrio.tif") as ds:
        assert ds.tags(ns="rio_overview") == {"resampling": "average"}


def test_update_in_place_keeps_domains(tmp_path):
    """r+ rewrites the file in terrio: GDAL's dataset domains stay, next to the new item."""
    write(rasterio, tmp_path / "src.tif", "GTiff")
    for mod, name in ((rasterio, "gdal"), (trio, "terrio")):
        shutil.copyfile(tmp_path / "src.tif", tmp_path / f"{name}.tif")
        with mod.open(tmp_path / f"{name}.tif", "r+") as ds:
            ds.update_tags(added="yes")
    assert gdal_view(tmp_path / "terrio.tif")[0][0] == gdal_view(tmp_path / "gdal.tif")[0][0]


@pytest.mark.xfail(
    strict=True,
    reason="terrio's GeoTIFF reader drops band domains (_tiff._apply_gdal_metadata) and "
    "DatasetReader.tags(bidx, ns) returns {}",
)
def test_band_domains_read_back(tmp_path):
    write(rasterio, tmp_path / "gdal.tif", "GTiff")
    with trio.open(tmp_path / "gdal.tif") as ds:
        assert ds.tags(1, ns="bar") == {"b": "3"}
