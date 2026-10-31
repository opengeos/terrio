"""terrio in a fork()ed child, after the parent used terrio's parallel paths.

Python's multiprocessing starts children with fork() on Linux, and an ingestion pipeline
converts every COG and vector file in such a child. A child has only the thread that forked:
terrio's thread pool must be started again there, never waited on. Before the fix, every child
below waited forever once its parent had read, written, warped, decoded JPEG XL or read
vectors in parallel.
"""

import multiprocessing as mp
import multiprocessing.queues
import os
import queue
import sys
from pathlib import Path

import numpy as np
import pytest

import terrio
import terrio.rio as rio
import terrio.rio.shutil
from terrio.rio.enums import Resampling
from terrio.rio.transform import from_origin
from terrio.rio.warp import reproject

# Linux, where multiprocessing forks by default and ingestion pipelines run; macOS forks unsafely
# once system frameworks have started threads, whatever terrio does.
pytestmark = pytest.mark.skipif(sys.platform != "linux", reason="fork() children on Linux")

# Large enough for every path to split its work over the pool: several strips, tiles, tile
# rows of the COG, output rows of the warp and records of the shapefile.
SIZE = 1024
# A child that is not done by then is hung (each finishes in about a second).
TIMEOUT = 60


def _data() -> np.ndarray:
    return (np.arange(3 * SIZE * SIZE, dtype=np.uint32).reshape(3, SIZE, SIZE) % 251).astype(
        "uint8"
    )


def _profile() -> dict:
    return dict(
        driver="GTiff",
        width=SIZE,
        height=SIZE,
        count=3,
        dtype="uint8",
        crs="EPSG:4326",
        transform=from_origin(10, 50, 1 / SIZE, 1 / SIZE),
    )


def _features() -> dict:
    return {
        "type": "FeatureCollection",
        "features": [
            {
                "type": "Feature",
                "properties": {"name": f"point {i}", "value": i},
                "geometry": {"type": "Point", "coordinates": [10 + i / 5000, 50 - i / 5000]},
            }
            for i in range(5000)
        ],
    }


def _read(d: Path) -> int:
    with rio.open(d / "strips.tif") as ds:
        a = ds.read()
    with rio.open(d / "tiled.tif") as ds:
        b = ds.read()
    return int(a.sum()) + int(b.sum())


def _cog(d: Path, name: str) -> int:
    out = d / name
    terrio.rio.shutil.copy(d / "strips.tif", out, driver="COG", compress="deflate")
    with rio.open(out) as ds:
        return int(ds.read().sum())


def _warp(d: Path) -> int:
    dst = np.zeros((3, SIZE, SIZE), dtype="uint8")
    reproject(
        _data(),
        dst,
        src_transform=_profile()["transform"],
        src_crs="EPSG:4326",
        dst_transform=from_origin(1113194.9, 6446275.8, 110.0, 110.0),
        dst_crs="EPSG:3857",
        resampling=Resampling.bilinear,
    )
    return int(dst.sum())


def _jxl(d: Path) -> int:
    # One JPEG XL tile, decoded on the calling thread by jxl-oxide, which uses rayon.
    with rio.open(d / "jxl.tif") as ds:
        return int(ds.read().sum())


def _vector(d: Path) -> int:
    ds = terrio.open_vector(str(d / "points.shp"))
    return sum(len(layer) for layer in ds.layers)


def _all(d: Path, cog: str) -> dict:
    return {
        "read": _read(d),
        "cog": _cog(d, cog),
        "warp": _warp(d),
        "jxl": _jxl(d),
        "vector": _vector(d),
    }


# The parent's only parallel work before it forks, one kind per test.
PARENT_WORK = {
    "read": _read,
    "cog": lambda d: _cog(d, "parent.tif"),
    "warp": _warp,
    "jxl": _jxl,
    "vector": _vector,
}


def _make(d: Path, expected: mp.queues.Queue) -> None:
    # In a spawned interpreter: the inputs, and the results every child must find.
    data = _data()
    with rio.open(d / "strips.tif", "w", **_profile()) as ds:
        ds.write(data)
    with rio.open(
        d / "tiled.tif",
        "w",
        tiled=True,
        blockxsize=256,
        blockysize=256,
        compress="deflate",
        **_profile(),
    ) as ds:
        ds.write(data)
    with rio.open(
        d / "jxl.tif",
        "w",
        tiled=True,
        blockxsize=SIZE,
        blockysize=SIZE,
        compress="jxl",
        **_profile(),
    ) as ds:
        ds.write(data)
    terrio.write_vector(_features(), str(d / "points.shp"), crs="EPSG:4326")
    expected.put(_all(d, "reference.tif"))


def _child(d: Path, results: mp.queues.Queue) -> None:
    # Everything a parent may have run in parallel, in the forked child.
    results.put(_all(d, f"child-{os.getpid()}.tif"))


def _parent(d: Path, work: str, out: mp.queues.Queue) -> None:
    # In a spawned interpreter, so that this parent's only parallel work is `work`, whatever ran
    # before in the test session: then it forks, as an ingestion pipeline does.
    PARENT_WORK[work](d)
    ctx = mp.get_context("fork")
    results = ctx.Queue()
    p = ctx.Process(target=_child, args=(d, results))
    p.start()
    try:
        got = results.get(timeout=TIMEOUT)
    except queue.Empty:
        got = None
    # A child that sent its results exits; one that did not by now is killed.
    p.join(TIMEOUT if got is not None else 0)
    if p.is_alive():
        p.kill()
        p.join()
    out.put((got, p.exitcode))


@pytest.fixture(scope="module")
def inputs(tmp_path_factory):
    d = tmp_path_factory.mktemp("fork")
    ctx = mp.get_context("spawn")
    expected = ctx.Queue()
    p = ctx.Process(target=_make, args=(d, expected))
    p.start()
    results = expected.get(timeout=TIMEOUT)
    p.join(TIMEOUT)
    assert p.exitcode == 0
    assert results["vector"] == 5000
    return d, results


@pytest.mark.parametrize("parent", list(PARENT_WORK))
def test_a_forked_child_runs_terrio_after_the_parent_did(inputs, parent):
    d, expected = inputs
    ctx = mp.get_context("spawn")
    out = ctx.Queue()
    p = ctx.Process(target=_parent, args=(d, parent, out))
    p.start()
    got, exitcode = out.get(timeout=3 * TIMEOUT)
    p.join(TIMEOUT)
    assert got is not None, (
        f"the child hung after the parent's {parent!r} (killed after {TIMEOUT} s)"
    )
    assert exitcode == 0
    assert got == expected
