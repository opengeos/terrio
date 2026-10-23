"""The `rio` command line of terrio.rio (`terrio-rio`, `python -m terrio.rio`), without rasterio.

`test_rio_vs_rasterio.py` runs the same commands as rasterio's `rio` when rasterio is installed
(the `rio cli` checks of `scripts/compare_rio_with_rasterio.py`) and compares the results.
"""

import json
import os
import subprocess
import sys
import sysconfig
from collections import OrderedDict
from pathlib import Path

import numpy as np
import pytest
from affine import Affine

import terrio
import terrio.rio as rio
from terrio.rio import cli
from terrio.rio.control import GroundControlPoint
from terrio.rio.transform import from_gcps

T = rio.transform.from_origin(500000, 4100000, 30, 30)


def run(*args, stdin=None, cwd=None):
    return subprocess.run(
        [sys.executable, "-m", "terrio.rio", *map(str, args)],
        input=stdin,
        capture_output=True,
        text=True,
        cwd=cwd,
    )


@pytest.fixture
def raster(tmp_path):
    path = tmp_path / "in.tif"
    yy, xx = np.mgrid[0:60, 0:80]
    data = np.stack([(xx + yy) % 7, (xx * 2) % 5]).astype("int16")
    data[:, :5, :5] = -1
    profile = dict(driver="GTiff", width=80, height=60, count=2, dtype="int16")
    profile.update(crs="EPSG:32611", transform=T, nodata=-1, tiled=True)
    profile.update(blockxsize=32, blockysize=32)
    with rio.open(path, "w", **profile) as dst:
        dst.write(data)
    return path


def test_version_and_help():
    r = run("--version")
    assert r.returncode == 0 and r.stdout.strip() == terrio.__version__
    r = run("--help")
    assert r.returncode == 0
    for name in ("info", "bounds", "warp", "calc", "edit-info"):
        assert f"  {name}" in r.stdout
    assert run("info", "--help").returncode == 0
    r = run("nope")
    assert r.returncode == 2 and "No such command" in r.stderr
    r = run("--gdal-version")
    assert r.returncode == 1 and "GDAL" in r.stderr


def test_console_script():
    name = "terrio-rio.exe" if os.name == "nt" else "terrio-rio"
    script = Path(sysconfig.get_path("scripts")) / name
    if not script.exists():
        pytest.skip(f"{script} is not installed (the package is not installed from a wheel)")
    r = subprocess.run([str(script), "--version"], capture_output=True, text=True)
    assert r.returncode == 0 and r.stdout.strip() == terrio.__version__


def test_info(raster):
    r = run("info", raster)
    assert r.returncode == 0, r.stderr
    info = json.loads(r.stdout)
    assert info["count"] == 2 and info["shape"] == [60, 80] and info["crs"] == "EPSG:32611"
    assert info["nodata"] == -1.0 and info["transform"] == list(T)
    assert info["bounds"] == [500000.0, 4098200.0, 502400.0, 4100000.0]
    assert run("info", raster, "--count").stdout == "2\n"
    assert run("info", raster, "--shape").stdout == "60 80\n"
    assert run("info", raster, "--crs").stdout == "EPSG:32611\n"
    assert run("info", raster, "--checksum", "-b", "2").returncode == 0
    r = run("info", "missing.tif")
    assert r.returncode == 1 and "missing.tif" in r.stderr


def test_bounds_blocks_shapes(raster):
    r = run("bounds", raster, "--projected")
    feat = json.loads(r.stdout)
    assert feat["bbox"] == [500000.0, 4098200.0, 502400.0, 4100000.0]
    assert feat["properties"]["filename"] == "in.tif"
    r = run("bounds", raster, "--bbox", "--feature")
    assert r.returncode == 2 and "may not be specified together" in r.stderr
    blocks = json.loads(run("blocks", raster, "--projected").stdout)
    assert len(blocks["features"]) == 6
    assert blocks["features"][0]["properties"]["window"] == {
        "col_off": 0,
        "row_off": 0,
        "width": 32,
        "height": 32,
    }
    shapes = run("shapes", raster, "--projected", "--collection").stdout
    fc = json.loads(shapes)
    assert {f["properties"]["val"] for f in fc["features"]} == set(range(7))


def test_sample_transform(raster):
    r = run("sample", raster, stdin="[500015, 4099985]\n[500300, 4099700]\n")
    assert r.stdout.split("\n")[:2] == ["[-1, -1]", "[6, 0]"]
    assert run("sample", raster, "[500300, 4099700]", "-b", "2..").stdout == "[0]\n"
    r = run("transform", "--dst-crs", "EPSG:32611", "--precision", "1", stdin="[-117, 37]\n")
    x, y = json.loads(r.stdout)
    assert abs(x - 500000) < 1 and abs(y - 4094872.4) < 0.01


def test_write_commands(raster, tmp_path):
    out = tmp_path / "out"
    out.mkdir()
    args = ("-t", "uint8", "--scale-offset", "1", "--co", "nodata=none")
    r = run("convert", raster, "a.tif", *args, cwd=out)
    assert r.returncode == 0, r.stderr
    with rio.open(out / "a.tif") as ds:
        assert ds.dtypes == ("uint8", "uint8") and int(ds.read(1)[30, 30]) == 5
    assert run("convert", raster, "a.tif", cwd=out).returncode == 1
    r = run("clip", raster, "b.tif", "--bounds", "500300 4098500 501000 4099700", cwd=out)
    assert r.returncode == 0, r.stderr
    with rio.open(out / "b.tif") as ds:
        assert ds.shape == (40, 23) and ds.transform == Affine(30, 0, 500300, 0, -30, 4099700)
    r = run("calc", "(+ (read 1 1) (read 1 2))", raster, "c.tif", cwd=out)
    assert r.returncode == 0, r.stderr
    with rio.open(out / "c.tif") as ds, rio.open(raster) as src:
        a = src.read()
        assert ds.count == 1
        assert np.array_equal(ds.read(1)[10:, 10:], (a[0] + a[1])[10:, 10:])
    r = run("edit-info", out / "a.tif", "--nodata", "255", "--tag", "k=v", cwd=out)
    assert r.returncode == 0, r.stderr
    with rio.open(out / "a.tif") as ds:
        assert ds.nodata == 255 and ds.tags()["k"] == "v"
    r = run("overview", out / "a.tif", "--build", "2,4", cwd=out)
    assert r.returncode == 0, r.stderr
    r = run("overview", out / "a.tif", "--ls", cwd=out)
    assert r.stdout.splitlines()[1] == "  Band 1: [2, 4] (method: 'unknown')"


def test_key_val_bounds_build():
    assert cli.key_val(["COMPRESS=LZW", "tiled=TRUE", "x=None"]) == {
        "compress": "lzw",
        "tiled": "true",
        "x": None,
    }
    with pytest.raises(cli.UsageError):
        cli.key_val(["nokey"])
    assert cli.parse_bounds("[1, 2, 3, 4]") == (1.0, 2.0, 3.0, 4.0)
    assert cli.parse_bounds("1 2 3 4") == (1.0, 2.0, 3.0, 4.0)
    with pytest.raises(cli.UsageError):
        cli.parse_bounds("1 2 3")
    assert cli.parse_build("2^1..4") == [2, 4, 8, 16]
    assert cli.parse_build("2,3") == [2, 3]
    assert cli.parse_build("auto") == "auto"
    assert cli.parse_indexes("..2", (1, 2, 3)) == [1, 2]
    assert cli.parse_indexes("2..", (1, 2, 3)) == [2, 3]
    assert cli.parse_indexes("1,3", (1, 2, 3)) == [1, 3]


def test_snuggs():
    a = np.arange(6).reshape(2, 3)
    b = np.ones((2, 3))
    ctx = OrderedDict(first=np.stack([a, a * 2]), second=np.stack([b]))
    funcs = {"read": lambda i, j=None, dtype=None: ev.lookup(i, j)}
    ev = cli.Snuggs(funcs, ctx)
    assert np.array_equal(ev.eval("(+ 2 (* 0.5 (read 1 2)))"), 2 + 0.5 * a * 2)
    assert np.array_equal(ev.eval("(- (read 1 1) 1 1)"), a - 2)
    assert np.array_equal(ev.eval("(& (> (read 1 1) 1) (< (read 1 1) 4))"), (a > 1) & (a < 4))
    assert ev.eval("(mean (read 2 1))") == 1.0
    assert ev.eval("(astype (read 1 1) 'float32')").dtype == np.float32
    assert ev.eval("(cumsum (read 1 1) :axis 0)")[1].tolist() == [3, 5, 7]
    assert ev.eval("(+ 1e2 -3 .5)") == 97.5
    assert np.array_equal(ev.eval("(+ first 1)"), ctx["first"] + 1)
    with pytest.raises(cli.ExpressionError, match="not defined"):
        ev.eval("(+ nope 1)")
    with pytest.raises(cli.ExpressionError):
        ev.eval("(+ 1 2")
    with pytest.raises(cli.ExpressionError, match="has no value"):
        ev.eval("(sum (read 1 1) :axis)")
    with pytest.raises(AttributeError):
        ev.eval("(open 'x.txt')")


def test_from_gcps():
    t = Affine(10.0, 1.5, 500000.0, 0.8, -10.0, 4100000.0)
    gcps = [
        GroundControlPoint(row=r, col=c, x=(t * (c, r))[0], y=(t * (c, r))[1])
        for r in (0, 40, 80, 120)
        for c in (0, 50, 100, 150)
    ]
    assert np.allclose(from_gcps(gcps), t, rtol=1e-12)
    two = [GroundControlPoint(row=r, col=c, x=c * 2.0, y=-r * 3.0) for r, c in ((0, 0), (10, 10))]
    assert from_gcps(two) == Affine(2.0, 0.0, 0.0, 0.0, -3.0, 0.0)
    with pytest.raises(ValueError):
        from_gcps(two[:1])


def test_shapes_sampling(raster):
    with rio.open(raster) as src:
        full = list(rio.features.dataset_features(src, 1, geographic=False))
        half = list(rio.features.dataset_features(src, 1, sampling=2, geographic=False))
    assert full and half and len(half) <= len(full)

    def extent(feats):
        b = np.array([f["bbox"] for f in feats])
        return (*b[:, :2].min(axis=0), *b[:, 2:].max(axis=0))

    # Decimated by 2, the shapes still cover the raster's valid region (in map coordinates).
    assert extent(full) == extent(half) == (500000.0, 4098200.0, 502400.0, 4100000.0)
    with rio.open(raster) as src, pytest.raises(ValueError):
        list(rio.features.dataset_features(src, 3))
