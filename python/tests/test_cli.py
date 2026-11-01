"""The ``terrio`` command the Python package installs (``terrio.__main__``)."""

import json
import os
import subprocess
import sys
import sysconfig
import urllib.error
import urllib.request
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


@pytest.mark.parametrize("path", [[], ["warp"], ["vsi", "sync"], ["vsi", "sozip", "create"]])
def test_json_usage_without_dataset_arguments(path):
    result = run(*path, "--json-usage")
    assert result.returncode == 0, result.stderr
    catalog = json.loads(result.stdout)
    assert catalog["schema_version"] == "0.1"
    assert catalog["unstable"] is True
    assert catalog["command"]["path"] == ["terrio", *path]
    assert catalog["command"]["arguments"]
    if not path:
        names = {c["name"] for c in catalog["command"]["subcommands"]}
        assert {"warp", "vsi", "lineref", "filter"} <= names
        assert not {"mcp", "ui", "help"} & names


def test_json_usage_literal_dataset_name(tmp_path):
    (tmp_path / "--json-usage").write_text(json.dumps(POINTS))
    result = run("vsi", "list", "--", "--json-usage", cwd=tmp_path)
    assert result.returncode == 0, result.stderr
    assert '"schema_version"' not in result.stdout
    assert "--json-usage" in result.stdout


@pytest.mark.parametrize("path", [[], ["vsi", "sync"], ["warp"]])
def test_json_usage_matches_published_schema(path):
    from jsonschema import Draft202012Validator

    schema = json.loads(
        (Path(__file__).resolve().parents[2] / "schemas/cli-usage-v0.1.schema.json").read_text()
    )
    Draft202012Validator.check_schema(schema)
    result = run(*path, "--json-usage")
    assert result.returncode == 0, result.stderr
    Draft202012Validator(schema).validate(json.loads(result.stdout))


def test_ui(tmp_path):
    """Serves the forms' API through ``python -m terrio``, whose commands run in this interpreter."""
    (tmp_path / "points.geojson").write_text(json.dumps(POINTS))
    server = subprocess.Popen(
        [sys.executable, "-m", "terrio", "ui", "--no-open"],
        cwd=tmp_path,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        text=True,
    )
    try:
        line = server.stdout.readline().strip()
        base, token = line.removeprefix("terrio ui: ").split("/#token=")
        headers = {"X-Terrio-Token": token}

        def run(*args):
            """Runs ``terrio info`` through the server and returns the decoded result."""
            body = json.dumps({"name": "info", "arguments": {"args": list(args)}})
            request = urllib.request.Request(f"{base}/api/run", body.encode(), headers)
            with urllib.request.urlopen(request, timeout=60) as reply:
                return json.load(reply)

        result = run("points.geojson", "--json")
        assert result["isError"] is False
        assert json.loads(result["content"][0]["text"])["type"] == "vector"
        with pytest.raises(urllib.error.HTTPError) as refused:
            urllib.request.urlopen(f"{base}/api/catalog", timeout=60)
        assert refused.value.code == 403
    finally:
        server.kill()
        server.wait()
        server.stdout.close()
