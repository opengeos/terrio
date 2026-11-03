"""terrio.render and the notebook display of rasters and rasterio-style datasets."""

import io
import json

import numpy as np
import pytest

import terrio
import terrio.rio as rasterio


def _decode(png: bytes) -> np.ndarray:
    """Decodes a PNG into an array of shape (rows, cols, 4).

    Args:
        png: The PNG bytes.

    Returns:
        numpy.ndarray: The RGBA pixels.
    """
    assert png.startswith(b"\x89PNG")
    with rasterio.open(io.BytesIO(png)) as dataset:
        return np.moveaxis(dataset.read(), 0, -1)


@pytest.fixture
def grid(tmp_path):
    """An ASCII grid with a nodata pixel."""
    path = tmp_path / "grid.asc"
    header = "ncols 4\nnrows 2\nxllcorner 0\nyllcorner 0\ncellsize 1\nNODATA_value -9\n"
    path.write_text(header + "1 2 3 4\n5 6 7 -9\n")
    return path


def test_render_draws_a_path_or_a_raster_with_nodata_transparent(grid):
    pixels = _decode(terrio.render(grid, colormap="viridis", stretch="minmax"))
    assert pixels.shape == (2, 4, 4)
    assert list(pixels[0, 0]) == [0x44, 0x01, 0x54, 255]
    assert pixels[1, 3, 3] == 0
    raster = terrio.open_raster(str(grid), lazy=True)
    assert _decode(terrio.render(raster, width=8)).shape == (4, 8, 4)


def test_render_refuses_bad_options(grid):
    for kwargs in ({"colormap": "nope"}, {"stretch": "5,1"}, {"bands": [1, 2]}):
        with pytest.raises(ValueError):
            terrio.render(grid, **kwargs)
    assert terrio.render(grid, layers=[]).startswith(b"\x89PNG")
    with pytest.raises(ValueError, match="vector"):
        terrio.render(terrio.open_raster(str(grid), lazy=True), attribute="v")


def test_render_draws_vector_layers_by_attribute(tmp_path):
    collection = {
        "type": "FeatureCollection",
        "features": [
            {
                "type": "Feature",
                "properties": {"v": value},
                "geometry": {
                    "type": "Polygon",
                    "coordinates": [[[x, 0], [x + 1, 0], [x + 1, 1], [x, 1], [x, 0]]],
                },
            }
            for x, value in ((0, 1.0), (2, 2.0))
        ],
    }
    path = tmp_path / "squares.geojson"
    path.write_text(json.dumps(collection))
    for source in (path, terrio.open_vector(str(path)), collection, json.dumps(collection)):
        pixels = _decode(
            terrio.render(source, max_size=30, attribute="v", colormap="gray", stretch="minmax")
        )
        assert pixels.shape == (10, 30, 4)
        assert list(pixels[5, 5]) == [0, 0, 0, 255]
        assert list(pixels[5, 25]) == [255, 255, 255, 255]
        assert pixels[5, 15, 3] == 0


def test_notebooks_display_rasters_and_rasterio_datasets(grid):
    raster = terrio.open_raster(str(grid), lazy=True)
    assert raster._repr_png_().startswith(b"\x89PNG")
    with rasterio.open(grid) as dataset:
        assert dataset._repr_png_().startswith(b"\x89PNG")
    assert dataset._repr_png_() is None
