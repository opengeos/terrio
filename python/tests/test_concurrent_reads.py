"""One lazy Raster read from several threads at once."""

import threading
from concurrent.futures import ThreadPoolExecutor

import numpy as np

import terrio


def write_tiled(path, seed=0):
    pixels = np.random.default_rng(seed).integers(0, 4000, (2, 640, 600), dtype="uint16")
    terrio.Raster.from_array(pixels, (0, 1, 0, 640, 0, -1), crs="EPSG:32631").write(
        str(path), compress="deflate", tile_size=64
    )
    return pixels


def read_together(raster, threads=8):
    """Whole reads of `raster`, started by `threads` threads at once so that they overlap."""
    start = threading.Barrier(threads, timeout=30)

    def read(_):
        start.wait()
        return raster.read()

    with ThreadPoolExecutor(threads) as ex:
        return list(ex.map(read, range(threads)))


def test_one_raster_read_from_threads_at_once_reads_what_one_thread_reads(tmp_path):
    path = str(tmp_path / "tiled.tif")
    pixels = write_tiled(path)
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


def test_overlapping_reads_after_chdir_read_the_file_opened(tmp_path, monkeypatch):
    (tmp_path / "a").mkdir()
    (tmp_path / "b").mkdir()
    pixels = write_tiled(tmp_path / "a" / "tiled.tif", seed=1)
    write_tiled(tmp_path / "b" / "tiled.tif", seed=2)  # same name, other pixels
    monkeypatch.chdir(tmp_path / "a")
    raster = terrio.open_raster("tiled.tif", lazy=True)
    monkeypatch.chdir(tmp_path / "b")
    for got in read_together(raster):
        np.testing.assert_array_equal(got, pixels)
    assert len(raster._readers) > 0  # the reads overlapped, so spare readers read too


def test_a_burst_of_reads_keeps_at_most_four_spare_readers(tmp_path):
    path = tmp_path / "tiled.tif"
    pixels = write_tiled(path)
    raster = terrio.open_raster(str(path), lazy=True)
    for got in read_together(raster, threads=12):
        np.testing.assert_array_equal(got, pixels)
    assert 0 < len(raster._readers) <= 4  # MAX_SPARE_READERS in python/src/raster.rs


def test_overlapping_reads_take_the_options_the_raster_was_opened_with(tmp_path):
    """A spare reader opened on a thread outside the `Env` the raster was opened in reads the
    dataset as the raster does: its `.msk` sidecar stays ignored under `EMPTY_DIR`."""
    from terrio import rio

    path = str(tmp_path / "tiled.tif")
    write_tiled(path)
    profile = dict(driver="GTiff", width=600, height=640, count=1, dtype="uint8")
    with rio.open(path + ".msk", "w", **profile) as dst:
        dst.write(np.zeros((640, 600), "uint8"), 1)
    with rio.Env(GDAL_DISABLE_READDIR_ON_OPEN="EMPTY_DIR"):
        raster = terrio.open_raster(path, lazy=True)
    threads = 8
    start = threading.Barrier(threads, timeout=30)

    def read_masks(_):
        # whole reads make the threads overlap, so spare readers open; masks come from them too
        start.wait()
        masks = []
        for _ in range(5):
            raster.read()
            masks.append(raster.read_mask())
        return masks

    with ThreadPoolExecutor(threads) as ex:
        masks = [m for ms in ex.map(read_masks, range(threads)) for m in ms]
    assert len(raster._readers) >= 1, "no read overlapped another"
    for mask in masks:
        assert mask is None or bool((np.asarray(mask) > 0).all())
