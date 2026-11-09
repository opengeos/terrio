"""Rasters written through `terrio.rio` without a transform or a CRS (rasterio not needed).

A JPEG 2000 file carries its georeferencing in a GeoJP2 box (a `uuid` box holding a small
GeoTIFF), and PNG, JPEG and WebP files in a world file (`.wld`) beside them. A raster with no
transform and no CRS has none to carry, so it is written without either, as a GeoTIFF is written
without a geotransform. One with a CRS alone keeps them, as before: the box or the CRS sidecar is
where that CRS goes.
"""

import os
import struct

import numpy as np
import pytest
from affine import Affine

import terrio.rio as rio

# The UUID of a GeoJP2 box.
GEOJP2 = bytes.fromhex("b14bf8bd083d4b43a5ae8cd7d5a6ce03")
T = Affine(30.0, 0.0, 500000.0, 0.0, -30.0, 4100000.0)
PIXELS = (np.arange(3 * 48 * 64) % 251).astype("uint8").reshape(3, 48, 64)
PROFILE = dict(width=64, height=48, count=3, dtype="uint8")
# The formats with a georeferencing of their own beside the pixels: (driver, extension, whether
# the pixels read back exactly).
FORMATS = [
    ("JP2", ".jp2", True),
    ("PNG", ".png", True),
    ("JPEG", ".jpg", False),
    ("WEBP", ".webp", True),
]
IDS = [f[0] for f in FORMATS]


def _boxes(data):
    """The top-level boxes of a JPEG 2000 file: `(type, payload)`."""
    out, i = [], 0
    while i + 8 <= len(data):
        size, kind = struct.unpack(">I4s", data[i : i + 8])
        header = 8
        if size == 1:
            size, header = struct.unpack(">Q", data[i + 8 : i + 16])[0], 16
        elif size == 0:
            size = len(data) - i
        out.append((kind, data[i + header : i + size]))
        i += size
    return out


def _has_geojp2_box(data):
    """Whether JPEG 2000 bytes hold a GeoJP2 box."""
    return any(kind == b"uuid" and payload.startswith(GEOJP2) for kind, payload in _boxes(data))


def _georeferencing_files(path):
    """What carries a written file's georeferencing: its GeoJP2 box, or its world file."""
    if path.endswith(".jp2"):
        with open(path, "rb") as f:
            return {"geojp2": _has_geojp2_box(f.read())}
    return {"wld": os.path.exists(os.path.splitext(path)[0] + ".wld")}


def _write(path, driver, **profile):
    with rio.open(path, "w", driver=driver, **{**PROFILE, **profile}) as dst:
        dst.write(PIXELS)


@pytest.mark.parametrize("driver, ext, exact", FORMATS, ids=IDS)
def test_a_raster_without_a_transform_or_a_crs_is_written_without_georeferencing(
    tmp_path, driver, ext, exact
):
    """No GeoJP2 box and no world file: they would declare the unit-pixel grid terrio uses for a
    raster without a transform, `(0, 1, 0, height, 0, -1)`, which nobody chose. The file reads
    back with the identity transform and no CRS."""
    path = str(tmp_path / f"a{ext}")
    _write(path, driver)
    assert _georeferencing_files(path) == ({"geojp2": False} if ext == ".jp2" else {"wld": False})
    assert sorted(os.listdir(tmp_path)) == [f"a{ext}"]
    with rio.open(path) as src:
        assert src.transform == Affine.identity() and src.crs is None
        if exact:
            assert np.array_equal(src.read(), PIXELS)


@pytest.mark.parametrize("driver, ext, exact", FORMATS, ids=IDS)
def test_a_raster_with_a_crs_and_no_transform_keeps_its_georeferencing(
    tmp_path, driver, ext, exact
):
    """A CRS without a transform is still written as before: in the GeoJP2 box of a JPEG 2000
    file, and in the `.aux.xml` sidecar of the others, beside a world file."""
    path = str(tmp_path / f"a{ext}")
    _write(path, driver, crs="EPSG:32611")
    assert _georeferencing_files(path) == ({"geojp2": True} if ext == ".jp2" else {"wld": True})
    with rio.open(path) as src:
        assert src.crs.to_epsg() == 32611


@pytest.mark.parametrize("driver, ext, exact", FORMATS, ids=IDS)
def test_a_georeferenced_raster_keeps_its_georeferencing(tmp_path, driver, ext, exact):
    """A raster with a transform is written with its GeoJP2 box or world file, as before."""
    path = str(tmp_path / f"a{ext}")
    _write(path, driver, transform=T)
    assert _georeferencing_files(path) == ({"geojp2": True} if ext == ".jp2" else {"wld": True})
    with rio.open(path) as src:
        assert src.transform == T


@pytest.mark.parametrize("driver, ext", [f[:2] for f in FORMATS if f[1] != ".jp2"], ids=IDS[1:])
def test_a_world_file_of_an_earlier_write_is_removed(tmp_path, driver, ext):
    """An image written over one that had a world file, without a transform or a CRS, leaves no
    world file: one left beside it would georeference it."""
    path = str(tmp_path / f"a{ext}")
    _write(path, driver, transform=T)
    assert _georeferencing_files(path) == {"wld": True}
    _write(path, driver)
    assert _georeferencing_files(path) == {"wld": False}
    with rio.open(path) as src:
        assert src.transform == Affine.identity()


def test_a_memoryfile_jpeg_2000_without_a_transform_or_a_crs_holds_no_geojp2_box():
    """A JPEG 2000 written to a `MemoryFile` (a tile server's path) holds no GeoJP2 box, and its
    bytes read back with the identity transform."""
    with rio.MemoryFile(ext=".jp2") as memfile:
        with memfile.open(driver="JP2", **PROFILE) as dst:
            dst.write(PIXELS)
        data = memfile.read()
        assert data.startswith(b"\x00\x00\x00\x0cjP  \r\n\x87\n")
        assert not _has_geojp2_box(data)
    with rio.MemoryFile(data, ext=".jp2") as memfile, memfile.open() as src:
        assert src.transform == Affine.identity() and src.crs is None
        assert np.array_equal(src.read(), PIXELS)
