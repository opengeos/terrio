"""The ``terrio`` command the Python package installs (``terrio.__main__``)."""

import json
import os
import subprocess
import sys
import sysconfig
from pathlib import Path

import pytest

import terrio

POINTS = {
    "type": "FeatureCollection",
    "features": [
        {
            "type": "Feature",
            "properties": {"name": "a"},
            "geometry": {"type": "Point", "coordinates": [1, 2]},
        },
        {
            "type": "Feature",
            "properties": {"name": "b"},
            "geometry": {"type": "Point", "coordinates": [3, 4]},
        },
    ],
}


def run(*args, cwd=None):
    """Runs ``python -m terrio`` with the arguments.

    Args:
        *args: The command-line arguments.
        cwd: The working directory.

    Returns:
        subprocess.CompletedProcess: The finished process, its output captured as text.
    """
    return subprocess.run(
        [sys.executable, "-m", "terrio", *args], capture_output=True, text=True, cwd=cwd
    )


def test_version():
    result = run("--version")
    assert result.returncode == 0
    assert result.stdout.strip() == f"terrio {terrio.__version__}"


def test_console_script():
    name = "terrio.exe" if os.name == "nt" else "terrio"
    script = Path(sysconfig.get_path("scripts")) / name
    if not script.exists():
        pytest.skip(f"{script} is not installed (the package is not installed from a wheel)")
    result = subprocess.run([str(script), "--version"], capture_output=True, text=True)
    assert result.returncode == 0
    assert result.stdout.strip() == f"terrio {terrio.__version__}"


def test_convert_and_info(tmp_path):
    (tmp_path / "points.geojson").write_text(json.dumps(POINTS))
    result = run("convert", "points.geojson", "points.gpkg", cwd=tmp_path)
    assert result.returncode == 0, result.stderr
    result = run("info", "points.gpkg", "--json", cwd=tmp_path)
    assert result.returncode == 0, result.stderr
    assert "points" in result.stdout
    assert (
        len(terrio.open_vector(str(tmp_path / "points.gpkg")).layers[0].to_geojson()["features"])
        == 2
    )


def test_exit_codes(tmp_path):
    usage = run("convert")
    assert usage.returncode == 2
    assert "Usage" in usage.stderr
    failure = run("info", str(tmp_path / "missing.tif"))
    assert failure.returncode == 1
    assert failure.stderr.startswith("terrio: ")


def test_mcp(tmp_path):
    (tmp_path / "points.geojson").write_text(json.dumps(POINTS))
    messages = [
        {"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {}},
        {"jsonrpc": "2.0", "id": 1, "method": "tools/list"},
        {
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "info",
                "arguments": {"args": [str(tmp_path / "points.geojson"), "--json"]},
            },
        },
    ]
    with subprocess.Popen(
        [sys.executable, "-m", "terrio", "mcp"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        text=True,
    ) as server:
        for m in messages:
            server.stdin.write(json.dumps(m) + "\n")
        server.stdin.flush()
        # Read every reply before closing standard input, which stops the commands running.
        replies = {r["id"]: r for r in (json.loads(server.stdout.readline()) for _ in messages)}
        server.stdin.close()
        assert server.wait() == 0
    assert replies[0]["result"]["serverInfo"]["name"] == "terrio"
    assert "convert" in [t["name"] for t in replies[1]["result"]["tools"]]
    # The tool ran `python -m terrio info`, with this interpreter.
    info = replies[2]["result"]
    assert not info["isError"], info
    assert "points" in info["content"][0]["text"]


def test_warp_dst_transform(tmp_path):
    import numpy as np

    src = str(tmp_path / "src.tif")
    values = np.arange(24, dtype="float32").reshape(1, 4, 6)
    terrio.Raster.from_array(values, (0.0, 1.0, 0.0, 4.0, 0.0, -1.0), "EPSG:32610").write(src)
    out = str(tmp_path / "rotated.tif")
    # A 90° rotation: output pixel (c, r) is source pixel (r, c).
    result = run(
        "warp", src, out, "--dst-transform", "0", "0", "1", "4", "-1", "0", "--ts", "4", "6"
    )
    assert result.returncode == 0, result.stderr
    r = terrio.open_raster(out)
    assert r.transform == (0.0, 0.0, 1.0, 4.0, -1.0, 0.0)
    assert np.array_equal(np.asarray(r.read())[0], values[0].T)
    result = run("warp", src, out, "--dst-transform", "0", "0", "1", "4", "-1", "0")
    assert result.returncode != 0 and "size" in result.stderr
