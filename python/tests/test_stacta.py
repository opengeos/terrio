"""STAC items with tiled assets (STACTA) on a small pyramid made in the tests."""

import json
import math

import numpy as np

import terrio
import terrio.rio as rasterio


def write_pyramid(tmp_path, templates=None):
    """Writes GeoTIFF tiles of two tile matrices and the STAC item describing them.

    Matrix 0 is one 16 x 16 tile over the west half of the world, matrix 1 two by two tiles over
    the same extent, as WorldCRS84Quad's.

    Args:
        tmp_path: The directory to write to.
        templates: The item's asset templates, by default one, `data`.

    Returns:
        tuple: The item's path, and the full-resolution pixels `(32, 32)`.
    """
    full = np.arange(32 * 32, dtype="int16").reshape(32, 32)
    for m, n, res in ((0, 1, 180 / 16), (1, 2, 90 / 16)):
        for r in range(n):
            for c in range(n):
                data = (
                    full[::2, ::2] if m == 0 else full[r * 16 : (r + 1) * 16, c * 16 : (c + 1) * 16]
                )
                transform = (-180 + c * 16 * res, res, 0.0, 90 - r * 16 * res, 0.0, -res)
                (tmp_path / str(m) / str(r)).mkdir(parents=True, exist_ok=True)
                terrio.Raster.from_array(
                    np.ascontiguousarray(data), transform, crs="EPSG:4326", nodata=-1
                ).write(str(tmp_path / str(m) / str(r) / f"{c}.tif"))
    deg = 2 * math.pi * 6378137 / 360

    def matrix(ident, res, n):
        return {
            "identifier": ident,
            "scaleDenominator": res * deg / 0.00028,
            "topLeftCorner": [-180, 90],
            "tileWidth": 16,
            "tileHeight": 16,
            "matrixWidth": n,
            "matrixHeight": n,
        }

    item = {
        "type": "Feature",
        "stac_version": "1.0.0",
        "stac_extensions": ["tiled-assets"],
        "id": "pyramid",
        "geometry": None,
        "properties": {
            "datetime": "2024-01-01T00:00:00Z",
            "tiles:tile_matrix_sets": {
                "Quad": {
                    "type": "TileMatrixSetType",
                    "identifier": "Quad",
                    "supportedCRS": "http://www.opengis.net/def/crs/OGC/1.3/CRS84",
                    "tileMatrix": [matrix("0", 180 / 16, 1), matrix("1", 90 / 16, 2)],
                }
            },
        },
        "asset_templates": templates
        or {"data": {"href": "./{TileMatrix}/{TileRow}/{TileCol}.tif"}},
    }
    path = tmp_path / "item.json"
    path.write_text(json.dumps(item))
    return path, full


def test_stacta_open_and_read(tmp_path):
    path, full = write_pyramid(tmp_path)
    r = terrio.open_raster(str(path), lazy=True)
    assert (r.width, r.height) == (32, 32)
    assert r.overviews == [(16, 16)]
    np.testing.assert_array_equal(r.read(1, window=(10, 12, 12, 8)), full[12:20, 10:22])
    np.testing.assert_array_equal(r.read(1, overview=0), full[::2, ::2])
    summary = terrio.info(str(path))
    assert summary["driver"] == "STACTA"
    assert ["datetime", "2024-01-01T00:00:00Z"] in [list(m) for m in summary["metadata"]]


def test_stacta_rio_and_subdatasets(tmp_path):
    templates = {
        "data": {"href": "./{TileMatrix}/{TileRow}/{TileCol}.tif"},
        "same": {"href": "./{TileMatrix}/{TileRow}/{TileCol}.tif"},
    }
    path, full = write_pyramid(tmp_path, templates)
    with rasterio.open(str(path)) as src:
        assert src.driver == "STACTA"
        assert src.subdatasets == [f'STACTA:"{path}":data', f'STACTA:"{path}":same']
    with rasterio.open(f'STACTA:"{path}":same') as src:
        assert (src.width, src.height, src.count, src.nodata) == (32, 32, 1, -1)
        assert src.overviews(1) == [2]
        np.testing.assert_array_equal(src.read(1, window=((14, 18), (14, 18))), full[14:18, 14:18])
