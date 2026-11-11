"""Compression levels through `terrio.rio`: `zlevel` for PNG, to a path and in a `MemoryFile`, and
for GeoTIFF and COG Deflate, `zstd_level` for GeoTIFF Zstandard; and `ZLEVEL` through
`terrio.write_raster`, `terrio.convert` and `terrio.rio.shutil.copy`. A level changes the bytes
written, not the pixels read back, and a level out of range is refused."""

import numpy as np
import pytest

import terrio
import terrio.rio as rio
from terrio.rio import shutil as rio_shutil
from terrio.rio.io import MemoryFile
from terrio.rio.transform import from_origin

SIZE = 256
_rng = np.random.default_rng(3)
_y, _x = np.mgrid[0:SIZE, 0:SIZE]
IMAGE = np.clip(
    np.stack([_x // 4, _y // 4, (_x + _y) // 8]) + _rng.normal(0, 3, (3, SIZE, SIZE)), 0, 255
).astype("uint8")
PROFILE = dict(width=SIZE, height=SIZE, count=3, dtype="uint8")
GEO = dict(crs="EPSG:3857", transform=from_origin(0, SIZE, 1, 1))


def png_in_memory(**options):
    with MemoryFile() as mem:
        with mem.open(driver="PNG", **PROFILE, **options) as dst:
            dst.write(IMAGE)
        data = mem.read()
    with MemoryFile(data) as mem, mem.open() as src:
        assert (src.read() == IMAGE).all()
    return data


def to_path(path, **options):
    with rio.open(path, "w", **PROFILE, **GEO, **options) as dst:
        dst.write(IMAGE)
    with rio.open(path) as src:
        assert (src.read() == IMAGE).all()
    return path.read_bytes()


WRITERS = {
    "PNG in memory": lambda tmp, **o: png_in_memory(**o),
    "PNG": lambda tmp, **o: to_path(tmp / "a.png", driver="PNG", **o),
    "GTiff DEFLATE": lambda tmp, **o: to_path(
        tmp / "a.tif", driver="GTiff", compress="deflate", **o
    ),
    "GTiff tiled DEFLATE": lambda tmp, **o: to_path(
        tmp / "t.tif", driver="GTiff", tiled=True, compress="deflate", **o
    ),
    "COG DEFLATE": lambda tmp, **o: to_path(tmp / "c.tif", driver="COG", compress="deflate", **o),
}


@pytest.mark.parametrize("writer", list(WRITERS))
def test_zlevel_changes_the_bytes_written(tmp_path, writer):
    write = WRITERS[writer]
    fastest, default, six, smallest = (
        write(tmp_path, zlevel=1),
        write(tmp_path),
        write(tmp_path, zlevel=6),
        write(tmp_path, zlevel=9),
    )
    assert default == six
    assert fastest != smallest
    assert len(smallest) <= len(fastest)


@pytest.mark.parametrize(
    ("writer", "option", "value"),
    [
        ("PNG in memory", "zlevel", 0),
        ("PNG", "zlevel", 10),
        ("GTiff DEFLATE", "zlevel", 10),
        ("COG DEFLATE", "zlevel", "fast"),
        ("GTiff DEFLATE", "zstd_level", 23),
    ],
)
def test_a_level_out_of_range_is_refused(tmp_path, writer, option, value):
    with pytest.raises(Exception, match=f"invalid {option.upper()} '{value}'"):
        WRITERS[writer](tmp_path, **{option: value})


OTHER_WRITERS = {
    "terrio.write_raster": lambda src, out, driver, compress, co: terrio.write_raster(
        terrio.open_raster(str(src)), str(out), driver=driver, compress=compress, options=co
    ),
    "terrio.convert": lambda src, out, driver, compress, co: terrio.convert(
        str(src), str(out), format=driver, compress=compress, options=co
    ),
    "terrio.rio.shutil.copy": lambda src, out, driver, compress, co: rio_shutil.copy(
        str(src),
        str(out),
        driver=driver,
        **({"compress": compress} if compress else {}),
        **{k.lower(): v for k, v in co.items()},
    ),
}


@pytest.mark.parametrize("writer", list(OTHER_WRITERS))
@pytest.mark.parametrize("driver", ["PNG", "GTiff"])
def test_zlevel_reaches_the_other_python_writers(tmp_path, writer, driver):
    """`terrio.write_raster(options=)`, `terrio.convert(options=)` and `terrio.rio.shutil.copy`
    take `ZLEVEL` too: level 6 without it, and levels 1 and 9 write other bytes."""
    src = tmp_path / "src.tif"
    to_path(src, driver="GTiff")
    compress, suffix = ("DEFLATE", "tif") if driver == "GTiff" else (None, "png")
    written = {}
    for zlevel in (None, "6", "1", "9"):
        out = tmp_path / f"{zlevel}.{suffix}"
        OTHER_WRITERS[writer](
            src, out, driver, compress, {} if zlevel is None else {"ZLEVEL": zlevel}
        )
        with rio.open(out) as back:
            assert (back.read() == IMAGE).all()
        written[zlevel] = out.read_bytes()
    assert written[None] == written["6"]
    assert written["1"] != written["9"]


@pytest.mark.parametrize("columns,rows", [(64, 64), (37, 53), (37, 1024)])
@pytest.mark.parametrize("driver", ["GTiff", "COG"])
def test_default_deflate_compacts_repeating_class_rows(tmp_path, columns, rows, driver):
    """Default level 6 retains long matches across runs and the 32 KiB window.

    GDAL 3.13.3 is the independent TIFF decoder used in conformance validation.
    The size bound allows metadata/overview variation, but catches the original
    12–20 KiB regression for these 1024-square categorical images.
    """
    y, x = np.mgrid[:1024, :1024]
    values = ((x // columns + y // rows) % 10).astype("uint8")
    path = tmp_path / "classes.tif"
    with rio.open(
        path,
        "w",
        driver=driver,
        width=1024,
        height=1024,
        count=1,
        dtype="uint8",
        crs="EPSG:32633",
        transform=from_origin(500000, 5800000, 10, 10),
        compress="DEFLATE",
        tiled=True,
        blockxsize=512,
        blockysize=512,
        blocksize=512,
    ) as dst:
        dst.write(values, 1)
    with rio.open(path) as src:
        np.testing.assert_array_equal(src.read(1), values)
        assert src.crs.to_epsg() == 32633
        assert src.transform == from_origin(500000, 5800000, 10, 10)
        assert src.nodata is None
        assert src.dtypes == ("uint8",)
        assert (src.read_masks(1) == 255).all()
    assert path.stat().st_size <= (11000 if driver == "COG" else 8500)
