"""One lazy Raster read from several threads at once."""

import threading
from concurrent.futures import ThreadPoolExecutor

import numpy as np

import terrio


def test_one_raster_read_from_threads_at_once_reads_what_one_thread_reads(tmp_path):
    rng = np.random.default_rng(0)
    pixels = rng.integers(0, 4000, (2, 640, 600), dtype="uint16")
    path = str(tmp_path / "tiled.tif")
    terrio.Raster.from_array(pixels, (0, 1, 0, 640, 0, -1), crs="EPSG:32631").write(
        path, compress="deflate", tile_size=64
    )
    raster = terrio.open_raster(path, lazy=True)
    windows = [(x, y, 150, 130) for y in range(0, 500, 70) for x in range(0, 450, 90)]
    threads = 8
    start = threading.Barrier(threads, timeout=30)

    def read(w):
        return raster.read(window=w)

    def first(w):  # every thread starts its first read together
        start.wait()
        return read(w)

    with ThreadPoolExecutor(threads) as ex:
        together = list(ex.map(first, windows[:threads]))
        concurrent = list(ex.map(read, windows * 3))
    for w, got in zip(windows[:threads] + windows * 3, together + concurrent, strict=True):
        x, y, width, height = w
        np.testing.assert_array_equal(got, pixels[:, y : y + height, x : x + width])
    # and the raster still reads on its own, as before
    np.testing.assert_array_equal(raster.read(band=2), pixels[1])
