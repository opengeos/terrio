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
