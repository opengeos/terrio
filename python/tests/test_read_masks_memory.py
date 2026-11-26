"""A nodata mask is computed a strip of rows at a time: reading the mask of a large band holds the
mask and small temporaries, not a boolean per pixel of the band besides. The peak counts what
tracemalloc sees, NumPy's arrays: the band's values, which Rust allocates, are not in it."""

import tracemalloc

import numpy as np

import terrio
import terrio.rio


def test_a_nodata_mask_holds_no_full_size_temporary(tmp_path):
    n = 4096
    values = np.arange(n * n, dtype=np.uint16).reshape(n, n)
    values[:, : n // 2] = 0
    path = str(tmp_path / "nodata.tif")
    gt = (0.0, 1.0, 0.0, float(n), 0.0, -1.0)
    terrio.Raster.from_array(values, gt, crs="EPSG:3857", nodata=0).write(
        path, driver="GTiff", tile_size=256
    )
    with terrio.rio.open(path) as src:
        tracemalloc.start()
        try:
            masks = src.read_masks()
            peak = tracemalloc.get_traced_memory()[1]
        finally:
            tracemalloc.stop()
    np.testing.assert_array_equal(masks[0], np.where(values == 0, 0, 255).astype(np.uint8))
    # The mask is n * n bytes; a boolean per pixel of the band besides doubled it.
    assert peak < n * n * 5 // 4, peak
