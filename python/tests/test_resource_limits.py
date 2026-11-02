"""Resource refusals have their own exception on the public Python surfaces."""

import json
import os
import subprocess
import sys

import terrio


def test_resource_limit_exception():
    assert terrio.ResourceLimitError is terrio.errors.ResourceLimitError
    assert issubclass(terrio.ResourceLimitError, terrio.TerrioError)
    assert issubclass(terrio.ResourceLimitError, MemoryError)
    assert not issubclass(terrio.ResourceLimitError, terrio.InvalidArgumentError)


def test_grib_resource_limit():
    # The fixture's real grid exceeds the one-point budget, before decoding it.
    from pathlib import Path

    fixture = Path(__file__).parents[2] / "crates/terrio/tests/data/grib/temperature.grib2"
    result = subprocess.run(
        [sys.executable, "-c", "import terrio, sys; terrio.open(sys.argv[1])", str(fixture)],
        env={**os.environ, "TERRIO_GRIB_MAX_POINTS": "1"},
        capture_output=True,
        text=True,
    )
    assert result.returncode != 0
    assert "ResourceLimitError" in result.stderr


def test_explicit_gdal_vector_budget(tmp_path, monkeypatch):
    import pytest

    if not any(
        driver["name"] == "GDAL" and driver["kind"] == "vector" and driver["read"]
        for driver in terrio.drivers()
    ):
        pytest.skip("requires the optional GDAL backend")

    path = tmp_path / "point.geojson"
    path.write_text(
        '{"type":"FeatureCollection","features":[{"type":"Feature",'
        '"properties":{"name":"point"},"geometry":{"type":"Point",'
        '"coordinates":[1,2]}}]}'
    )
    name = f"GDAL:{path}"
    monkeypatch.setenv("TERRIO_GDAL_MAX_VECTOR_BYTES", "1048576")
    terrio.open_vector(name)
    monkeypatch.setenv("TERRIO_GDAL_MAX_VECTOR_BYTES", "1")
    for read in (terrio.open, terrio.open_vector):
        with pytest.raises(terrio.ResourceLimitError, match="TERRIO_GDAL_MAX_VECTOR_BYTES=1"):
            read(name)
    for invalid in ("0", "-1", "nonsense"):
        monkeypatch.setenv("TERRIO_GDAL_MAX_VECTOR_BYTES", invalid)
        with pytest.raises(terrio.InvalidArgumentError, match="positive integer"):
            terrio.open_vector(name)
    monkeypatch.setenv("TERRIO_GDAL_MAX_VECTOR_BYTES", "1048576")
    assert terrio.open_vector(name).driver == "GDAL"

    # Filtering happens before the cheap count or incremental dense-cell budget check.
    point = json.loads(path.read_text())["features"][0]
    path.write_text(json.dumps({"type": "FeatureCollection", "features": [point] * 1000}))
    monkeypatch.setenv("TERRIO_GDAL_MAX_VECTOR_BYTES", "10000")
    with pytest.raises(terrio.ResourceLimitError):
        terrio.open_vector(name)
    filtered = terrio.open_vector(name, bbox=(10, 10, 20, 20))
    assert len(filtered[0]) == 0


def test_sparse_esrijson_resource_limit_from_path_and_bytes(tmp_path):
    import pytest

    doc = {
        "fields": [{"name": f"f{i}", "type": "esriFieldTypeString"} for i in range(1000)],
        "features": [{}] * 40000,
    }
    payload = json.dumps(doc).encode()
    path = tmp_path / "sparse.json"
    path.write_bytes(payload)
    with pytest.raises(terrio.ResourceLimitError, match="features of 1000 fields"):
        terrio.open_vector(str(path))
    with pytest.raises(terrio.ResourceLimitError, match="features of 1000 fields"):
        terrio.open_bytes(payload, filename="sparse.json")


def test_jpeg_allocation_refusal_is_a_memory_error(tmp_path):
    from test_hardening import assert_raises_allocation_error, read_limited

    path = tmp_path / "big.jpg"
    path.write_bytes(bytes.fromhex("FFD8 FFC0 000B 08 8000 FFFF 01 01 11 00 FFD9"))
    result = read_limited("eager", path)
    assert_raises_allocation_error(result)
    assert result.stdout.startswith("RAISED ResourceLimitError:"), result.stdout
