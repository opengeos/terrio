"""TIFF block metadata agrees with stored tables and GDAL 3.12.2 without pixel reads."""

import struct

import numpy as np
import pytest

import terrio
import terrio.rio as rio

rasterio = pytest.importorskip("rasterio")


def block_tables(path):
    """Read classic TIFF or BigTIFF chunk arrays independently of either raster reader.

    Args:
        path: Local TIFF path.

    Returns:
        tuple: Offsets and byte counts from the first IFD.
    """
    raw = path.read_bytes()
    order = "<" if raw[:2] == b"II" else ">"
    big = struct.unpack_from(order + "H", raw, 2)[0] == 43
    field = 8 if big else 4
    offset = struct.unpack_from(order + ("Q" if big else "I"), raw, 8 if big else 4)[0]
    count = struct.unpack_from(order + ("Q" if big else "H"), raw, offset)[0]
    start = offset + (8 if big else 2)
    arrays = {}
    for index in range(count):
        entry = start + index * (20 if big else 12)
        tag, kind, length = struct.unpack_from(order + ("HHQ" if big else "HHI"), raw, entry)
        if tag not in {273, 279, 324, 325}:
            continue
        code = {3: "H", 4: "I", 16: "Q"}[kind]
        size = struct.calcsize(code)
        location = entry + (12 if big else 8)
        if size * length > field:
            location = struct.unpack_from(order + ("Q" if big else "I"), raw, location)[0]
        arrays[tag] = struct.unpack_from(order + code * length, raw, location)
    return arrays.get(324, arrays.get(273)), arrays.get(325, arrays.get(279))


@pytest.mark.parametrize("tiled", [False, True])
@pytest.mark.parametrize("interleave", ["pixel", "band"])
@pytest.mark.parametrize("bigtiff", ["YES", "NO"])
def test_block_items_match_stored_tables(tmp_path, tiled, interleave, bigtiff):
    """Every band's block addresses match the file, including edge tiles and strips.

    Args:
        tmp_path: Temporary directory.
        tiled: Use tiles rather than strips.
        interleave: Pixel or band organization.
        bigtiff: Force BigTIFF or classic TIFF.
    """
    path = tmp_path / "blocks.tif"
    values = np.arange(2 * 35 * 39, dtype="uint16").reshape(2, 35, 39)
    with rasterio.open(
        path,
        "w",
        driver="GTiff",
        width=39,
        height=35,
        count=2,
        dtype="uint16",
        tiled=tiled,
        blockxsize=16 if tiled else 39,
        blockysize=16,
        compress="deflate",
        interleave=interleave,
        BIGTIFF=bigtiff,
        transform=rasterio.transform.from_origin(10, 46, 1, 1),
    ) as dst:
        dst.write(values)
    offsets, sizes = block_tables(path)
    with rio.open(path) as actual, rasterio.open(path) as reference:
        height, width = reference.block_shapes[0]
        across, rows = (39 + width - 1) // width, (35 + height - 1) // height
        for band in [1, 2]:
            for y in range(rows):
                for x in range(across):
                    index = (
                        ((band - 1) * rows if interleave == "band" else 0) * across + y * across + x
                    )
                    for name, expected in [("OFFSET", offsets[index]), ("SIZE", sizes[index])]:
                        key = f"BLOCK_{name}_{x}_{y}"
                        assert actual.get_tag_item(key, "TIFF", bidx=band) == str(expected)
                        assert actual.get_tag_item(
                            key, "TIFF", bidx=band
                        ) == reference.get_tag_item(key, "TIFF", bidx=band)
            assert not any(key.startswith("BLOCK_") for key in actual.tags(band, "TIFF"))
        for x, y in [(across, 0), (0, rows), (-1, 0), (2**100, 0)]:
            assert actual.get_tag_item(f"BLOCK_OFFSET_{x}_{y}", "TIFF", bidx=1) is None
        assert actual.get_tag_item("BLOCK_OFFSET_0_0", "TIFF") is None
        with pytest.raises(IndexError):
            actual.get_tag_item("BLOCK_OFFSET_0_0", "TIFF", bidx=3)
        np.testing.assert_array_equal(actual.read(), values)
    with rio.MemoryFile(path.read_bytes()) as memory, memory.open() as actual:
        assert actual.get_tag_item("BLOCK_OFFSET_0_0", "TIFF", bidx=1) == str(offsets[0])


def test_block_queries_over_http_do_not_fetch_pixels(tmp_path, record_property):
    """A large COG's repeated block queries only fetch its header and chunk tables.

    Args:
        tmp_path: Temporary directory.
        record_property: Record measured transfer sizes in the test report.
    """
    from rasterio.shutil import copy
    from test_remote_reads import Server

    source = tmp_path / "source.tif"
    path = tmp_path / "blocks.tif"
    values = np.random.default_rng(42).integers(0, 256, (1, 1024, 1024), dtype="uint8")
    with rasterio.open(
        source,
        "w",
        driver="GTiff",
        width=1024,
        height=1024,
        count=1,
        dtype="uint8",
        transform=rasterio.transform.from_origin(10, 46, 1, 1),
    ) as dst:
        dst.write(values)
    copy(source, path, driver="COG", blocksize=128, compress="deflate")
    offsets, sizes = block_tables(path)
    server = Server(tmp_path)
    terrio.clear_http_cache()
    try:
        with rio.open(server.url + "/blocks.tif") as actual:
            for index in range(len(offsets)):
                x, y = index % 8, index // 8
                assert actual.get_tag_item(f"BLOCK_OFFSET_{x}_{y}", "TIFF", bidx=1) == str(
                    offsets[index]
                )
                assert actual.get_tag_item(f"BLOCK_SIZE_{x}_{y}", "TIFF", bidx=1) == str(
                    sizes[index]
                )
            before = server.bytes_sent()
            requests = len(server.log)
            for _ in range(10):
                actual.get_tag_item("BLOCK_OFFSET_0_0", "TIFF", bidx=1)
            assert server.bytes_sent() == before
            assert len(server.log) == requests
            assert before < path.stat().st_size // 4
            record_property("bytes_transferred", before)
            record_property("file_bytes", path.stat().st_size)
            record_property("requests", requests)
            assert all(row[2] == 206 for row in server.log if row[0] == "GET" and row[2] != 404)
    finally:
        server.close()
        terrio.clear_http_cache()


@pytest.mark.parametrize("external", [False, True])
def test_overview_block_items_match_gdal(tmp_path, external):
    """Overview indices and overview-as-dataset handles use their own backing tables.

    Args:
        tmp_path: Temporary directory.
        external: Store overviews in a sidecar rather than the base TIFF.
    """
    gdal = pytest.importorskip("osgeo.gdal")
    path = tmp_path / "overviews.tif"
    values = np.arange(2 * 128 * 128, dtype="uint16").reshape(2, 128, 128)
    with rasterio.open(
        path,
        "w",
        driver="GTiff",
        width=128,
        height=128,
        count=2,
        dtype="uint16",
        tiled=True,
        blockxsize=32,
        blockysize=32,
        interleave="band",
        transform=rasterio.transform.from_origin(10, 46, 1, 1),
    ) as dst:
        dst.write(values)
    dataset = gdal.Open(str(path), gdal.GA_ReadOnly if external else gdal.GA_Update)
    assert dataset.BuildOverviews("NEAREST", [2, 4]) == 0
    dataset = None
    assert path.with_suffix(".tif.ovr").exists() == external
    with rio.open(path) as actual, rasterio.open(path) as reference:
        for overview in [0, 1]:
            for band in [1, 2]:
                for key in ["BLOCK_OFFSET_0_0", "BLOCK_SIZE_0_0"]:
                    expected = reference.get_tag_item(key, "TIFF", bidx=band, ovr=overview)
                    assert expected is not None
                    assert actual.get_tag_item(key, "TIFF", bidx=band, ovr=overview) == expected
                    with rio.open(path, overview_level=overview) as selected:
                        assert selected.get_tag_item(key, "TIFF", bidx=band) == expected
        assert actual.get_tag_item("BLOCK_SIZE_0_0", "TIFF", bidx=1, ovr=20) is None


def test_complex_band_block_items_use_the_stored_plane(tmp_path):
    """Two complex components map to one stored planar TIFF band.

    Args:
        tmp_path: Temporary directory.
    """
    path = tmp_path / "complex.tif"
    values = np.full((2, 35, 39), 1 + 2j, dtype="complex64")
    with rasterio.open(
        path,
        "w",
        driver="GTiff",
        width=39,
        height=35,
        count=2,
        dtype="complex64",
        tiled=True,
        blockxsize=16,
        blockysize=16,
        interleave="band",
        transform=rasterio.transform.from_origin(10, 46, 1, 1),
    ) as dst:
        dst.write(values)
    with rio.open(path) as actual, rasterio.open(path) as reference:
        for band in [1, 2]:
            for key in ["BLOCK_OFFSET_0_0", "BLOCK_SIZE_2_2"]:
                assert actual.get_tag_item(key, "TIFF", bidx=band) == reference.get_tag_item(
                    key, "TIFF", bidx=band
                )


def test_real_tiff_block_items_match_gdal(data_dir):
    """Real Landsat storage addresses agree exactly with GDAL.

    Args:
        data_dir: Real-data directory.
    """
    path = data_dir / "landsat7.tif"
    if not path.exists():
        pytest.skip("needs landsat7.tif")
    with rio.open(path) as actual, rasterio.open(path) as reference:
        for band in reference.indexes:
            height, width = reference.block_shapes[band - 1]
            across = (reference.width + width - 1) // width
            rows = (reference.height + height - 1) // height
            for x, y in [(0, 0), (across - 1, rows - 1)]:
                for name in ["OFFSET", "SIZE"]:
                    key = f"BLOCK_{name}_{x}_{y}"
                    assert actual.get_tag_item(key, "TIFF", bidx=band) == reference.get_tag_item(
                        key, "TIFF", bidx=band
                    )
