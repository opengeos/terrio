"""Catalog APIs on in-memory sources, including the WASM-compatible input model."""

import numpy as np
import pytest

import terrio


def index():
    return {
        "type": "FeatureCollection",
        "features": [
            {
                "type": "Feature",
                "properties": {"location": name, "z": z},
                "geometry": {
                    "type": "Polygon",
                    "coordinates": [[[x, 0], [x + 4, 0], [x + 4, 4], [x, 4], [x, 0]]],
                },
            }
            for name, x, z in [("left", 0, 2), ("right", 2, 1), ("missing", 100, 3)]
        ],
    }


def test_catalog_windows_and_sort_order():
    left = terrio.Raster.from_array(
        np.full((4, 4), 10, dtype="float32"), (0, 1, 0, 4, 0, -1), crs="EPSG:4326"
    )
    right = terrio.Raster.from_array(
        np.full((4, 4), 20, dtype="float32"), (2, 1, 0, 4, 0, -1), crs="EPSG:4326"
    )
    catalog = terrio.open_catalog(
        index(),
        {"left": left, "right": right},
        tr=(1, 1),
        te=(0, 0, 6, 4),
        sort_field="z",
        cache_size=1,
    )
    assert (catalog.width, catalog.height, catalog.count) == (6, 4, 1)
    np.testing.assert_array_equal(catalog.read(1)[0], [10, 10, 10, 10, 20, 20])
    np.testing.assert_array_equal(catalog.read(1, window=(4, 0, 2, 2)), np.full((2, 2), 20))
    # The unselected missing source is never opened.
    assert left.read(1)[0, 0] == 10
    with pytest.raises(ValueError, match="positive"):
        terrio.open_catalog(index(), {"left": left}, cache_size=0)


def test_catalog_coverage_mask():
    left = terrio.Raster.from_array(
        np.full((4, 4), 10, dtype="float32"), (0, 1, 0, 4, 0, -1), crs="EPSG:4326"
    )
    catalog = terrio.open_catalog(
        index(), {"left": left}, tr=(1, 1), te=(0, 0, 8, 4), bands=1, ot="Float32"
    )
    # The unavailable right tile is outside this mask window.
    np.testing.assert_array_equal(catalog.read_mask(window=(0, 0, 2, 2)), np.full((2, 2), 255))
    np.testing.assert_array_equal(catalog.read_mask(window=(6, 0, 2, 2)), np.zeros((2, 2)))
