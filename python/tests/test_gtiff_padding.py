"""The padding of partial edge tiles is GDAL's: zeros, masked in LERC blobs of one float sample.

GDAL (rasterio) is the oracle. Each edge tile is read as stored (``BLOCK_OFFSET_*`` from GDAL's
``TIFF`` metadata domain). Uncompressed tiles must hold GDAL's padding bytes; for LERC, the
blob's mask (parsed from its Lerc2 header) must leave the same pixels valid as GDAL's.
"""

import struct

import numpy as np
import pytest

import terrio

rasterio = pytest.importorskip("rasterio")
rasterio_shutil = pytest.importorskip("rasterio.shutil")
MemoryFile = rasterio.io.MemoryFile

W, H, TILE = 300, 200, 128
GT = (500000.0, 10.0, 0.0, 4000000.0, 0.0, -10.0)
CASES = [
    ("float32", float("nan"), 1, "pixel"),
    ("float32", -9999.0, 1, "pixel"),
    ("float32", None, 1, "pixel"),
    ("float64", float("nan"), 1, "pixel"),
    ("float32", float("nan"), 2, "band"),
    ("float32", None, 2, "pixel"),
    ("uint16", 65535, 1, "pixel"),
    ("int16", None, 1, "pixel"),
    ("uint8", 255, 1, "pixel"),
]


def _array(dtype, nodata, bands):
    y, x = np.mgrid[0:H, 0:W]
    a = np.stack([1 + (x + y + 3 * b) % 97 for b in range(bands)]).astype(dtype)
    if nodata is not None:
        a[:, 40:60, 50:90] = nodata
        a[:, H - 10 :, :] = nodata  # nodata along the bottom edge
    return a


def _write_both(tmp_path, dtype, nodata, bands, interleave, driver, compress):
    a = _array(dtype, nodata, bands)
    gdal_path, terrio_path = tmp_path / "gdal.tif", tmp_path / "terrio.tif"
    profile = dict(
        driver="GTiff",
        width=W,
        height=H,
        count=bands,
        dtype=dtype,
        nodata=nodata,
        crs="EPSG:32615",
        transform=rasterio.Affine.from_gdal(*GT),
    )
    with MemoryFile() as mem, mem.open(**profile) as src:
        src.write(a)
        if driver == "COG":
            rasterio_shutil.copy(
                src,
                gdal_path,
                driver="COG",
                compress=compress,
                blocksize=TILE,
                overview_resampling="nearest",
                interleave=interleave,
            )
        else:
            rasterio_shutil.copy(
                src,
                gdal_path,
                driver="GTiff",
                tiled=True,
                blockxsize=TILE,
                blockysize=TILE,
                compress=compress,
                interleave=interleave,
            )
    options = {"INTERLEAVE": "BAND"} if interleave == "band" else {}
    raster = terrio.Raster.from_array(a, GT, crs="EPSG:32615", nodata=nodata)
    if driver == "COG":
        raster.write(
            str(terrio_path),
            driver="COG",
            compress=compress,
            tile_size=TILE,
            overview_resampling="nearest",
            options=options,
        )
    else:
        raster.write(
            str(terrio_path),
            driver="GTiff",
            compress=compress,
            tile_size=TILE,
            options=dict(options, TILED="YES"),
        )
    return gdal_path, terrio_path


def _edge_tiles(path):
    """(level, band, x, y, stored bytes, width, height) of each partial edge tile, every level."""
    out = []
    with rasterio.open(path) as d:
        levels = [None] + list(range(len(d.overviews(1))))
    for level in levels:
        with rasterio.open(path, overview_level=level) as d, open(path, "rb") as f:
            across, down = -(-d.width // TILE), -(-d.height // TILE)
            for band in range(1, d.count + 1):
                for ty in range(down):
                    for tx in range(across):
                        if tx < across - 1 and ty < down - 1:
                            continue
                        off = d.get_tag_item(f"BLOCK_OFFSET_{tx}_{ty}", "TIFF", bidx=band)
                        size = d.get_tag_item(f"BLOCK_SIZE_{tx}_{ty}", "TIFF", bidx=band)
                        f.seek(int(off))
                        out.append((level, band, tx, ty, f.read(int(size)), d.width, d.height))
            if d.interleaving == rasterio.enums.Interleaving.pixel:
                # One tile holds every band: keep the first band's.
                out = [t for t in out if t[0] != level or t[1] == 1]
    return out


def _lerc_valid(blob):
    """Read a Lerc2 blob's valid-pixel mask.

    Args:
        blob: Encoded Lerc2 bytes.

    Returns:
        A rows-by-columns boolean mask, or None when every pixel is valid.
    """
    assert blob[:6] == b"Lerc2 "
    version = struct.unpack_from("<i", blob, 6)[0]
    # Lerc2 header layout, as lerc-reader 0.5.2's lerc2::parse reads it:
    # six-byte magic, int32 version, then uint32 checksum from Lerc2 v3 onward.
    pos = 10 + (4 if version >= 3 else 0)
    rows, cols = struct.unpack_from("<II", blob, pos)
    # nRows and nCols; Lerc2 v4 adds the uint32 depth (values per pixel).
    pos += 8 + (4 if version >= 4 else 0)
    valid = struct.unpack_from("<I", blob, pos)[0]
    # numValidPixel, microBlockSize, blobSize and imageType (four 32-bit fields).
    # Lerc2 v6 adds nBlobsMore (int32) and four flag/reserved bytes.
    pos += 16 + (8 if version >= 6 else 0)
    # maxZError, zMin and zMax (three doubles). Lerc2 v6 adds the encoded and
    # original nodata values (two doubles). The mask byte count follows.
    pos += 24 + (16 if version >= 6 else 0)
    if valid == rows * cols:
        return None
    if valid == 0:
        return np.zeros((rows, cols), bool)
    n = struct.unpack_from("<I", blob, pos)[0]
    rle, out, i = blob[pos + 4 : pos + 4 + n], bytearray(), 0
    while True:
        count = struct.unpack_from("<h", rle, i)[0]
        i += 2
        if count == -32768:
            break
        if count > 0:
            out += rle[i : i + count]
            i += count
        else:
            out += bytes([rle[i]]) * -count
            i += 1
    bits = np.unpackbits(np.frombuffer(bytes(out), np.uint8))[: rows * cols]
    return bits.reshape(rows, cols).astype(bool)


def _padding(width, height, tx, ty):
    pad = np.ones((TILE, TILE), bool)
    pad[: min(TILE, height - ty * TILE), : min(TILE, width - tx * TILE)] = False
    return pad


@pytest.mark.parametrize("driver", ["GTiff", "COG"])
@pytest.mark.parametrize("case", CASES, ids=lambda c: f"{c[0]}-{c[1]}-x{c[2]}-{c[3]}")
def test_lerc_masks_the_padding_as_gdal(tmp_path, case, driver):
    gdal_path, terrio_path = _write_both(tmp_path, *case, driver, "LERC")
    gdal_tiles, terrio_tiles = _edge_tiles(gdal_path), _edge_tiles(terrio_path)
    assert [t[:4] for t in gdal_tiles] == [t[:4] for t in terrio_tiles]
    for g, t in zip(gdal_tiles, terrio_tiles, strict=True):
        pad = _padding(g[5], g[6], g[2], g[3])
        gv, tv = _lerc_valid(g[4]), _lerc_valid(t[4])
        gv = np.ones((TILE, TILE), bool) if gv is None else gv
        tv = np.ones((TILE, TILE), bool) if tv is None else tv
        np.testing.assert_array_equal(
            tv[pad], gv[pad], err_msg=f"level {g[0]} band {g[1]} tile {g[2]},{g[3]}"
        )
        if g[0] is None:
            # Inside, at full resolution (overview pixels follow each library's resampling).
            np.testing.assert_array_equal(
                tv[~pad], gv[~pad], err_msg=f"band {g[1]} tile {g[2]},{g[3]}"
            )
    # The pixels inside are unchanged.
    with rasterio.open(gdal_path) as g, rasterio.open(terrio_path) as t:
        np.testing.assert_array_equal(t.read(), g.read())


@pytest.mark.parametrize("driver", ["GTiff", "COG"])
@pytest.mark.parametrize("case", CASES, ids=lambda c: f"{c[0]}-{c[1]}-x{c[2]}-{c[3]}")
def test_uncompressed_padding_matches_gdal(tmp_path, case, driver):
    """Check that uncompressed partial tiles use GDAL's zero padding."""
    gdal_path, terrio_path = _write_both(tmp_path, *case, driver, "NONE")
    gdal_tiles, terrio_tiles = _edge_tiles(gdal_path), _edge_tiles(terrio_path)
    assert [t[:4] for t in gdal_tiles] == [t[:4] for t in terrio_tiles]
    dtype = np.dtype(case[0]).newbyteorder("<")
    for g, t in zip(gdal_tiles, terrio_tiles, strict=True):
        pad = _padding(g[5], g[6], g[2], g[3])
        gv = np.frombuffer(g[4], dtype).reshape(TILE, TILE, -1)
        tv = np.frombuffer(t[4], dtype).reshape(TILE, TILE, -1)
        assert not tv[pad].any(), "zeros in the padding"
        np.testing.assert_array_equal(tv[pad], gv[pad])
    with rasterio.open(gdal_path) as g, rasterio.open(terrio_path) as t:
        np.testing.assert_array_equal(t.read(), g.read())
