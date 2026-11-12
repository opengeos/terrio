"""Resource refusals have their own exception on the public Python surfaces."""

import json
import os
import subprocess
import sys

import pytest

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
    with pytest.raises(terrio.ResourceLimitError, match="ESRIPBF:.*features of 1000 fields"):
        terrio.open_vector(f"ESRIPBF:{path}")


def test_jpeg_allocation_refusal_is_a_memory_error(tmp_path):
    from test_hardening import assert_raises_allocation_error, read_limited

    path = tmp_path / "big.jpg"
    path.write_bytes(bytes.fromhex("FFD8 FFC0 000B 08 8000 FFFF 01 01 11 00 FFD9"))
    result = read_limited("eager", path)
    assert_raises_allocation_error(result)
    assert result.stdout.startswith("RAISED ResourceLimitError:"), result.stdout


def test_archive_policy_refusal_is_terminal(tmp_path, monkeypatch):
    import zipfile

    import pytest

    path = tmp_path / "points.zip"
    point = '{"type":"FeatureCollection","features":[]}'
    with zipfile.ZipFile(path, "w") as archive:
        archive.writestr("a.geojson", point)
        archive.writestr("b.geojson", point)
    monkeypatch.setenv("TERRIO_ZIP_MAX_ENTRIES", "1")
    with pytest.raises(terrio.ResourceLimitError, match="entries"):
        terrio.open_vector(f"/vsizip/{path}/a.geojson")


def test_pcraster_unknown_suffix_preserves_values_and_declaration_limits(tmp_path):
    """Bounded CSF recognition keeps unknown-suffix fallback support and its logical limit."""
    import numpy as np

    import terrio.rio as rio

    values = np.ones((1024, 1024), dtype="uint8")
    values[0] = np.arange(1024, dtype="uint16") % 100
    transform = (10, 1 / 1024, 0, 48, 0, -1 / 1024)
    source = tmp_path / "source.map"
    terrio.Raster.from_array(values, transform=transform).write(str(source))
    unknown = tmp_path / "binary.unknown"
    unknown.write_bytes(source.read_bytes())
    for limit in ("", str(values.nbytes)):
        with rio.Env(TERRIO_WBRASTER_FALLBACK="YES", TERRIO_MAX_DECLARED_BYTES=limit):
            raster = terrio.open_raster(str(unknown), lazy=False)
            np.testing.assert_array_equal(raster.read(1), values)
            assert raster.read(1).dtype == values.dtype
            assert raster.transform == transform
    with rio.Env(TERRIO_WBRASTER_FALLBACK="YES", TERRIO_MAX_DECLARED_BYTES="1"):
        with pytest.raises(terrio.ResourceLimitError, match="TERRIO_MAX_DECLARED_BYTES"):
            terrio.open_raster(str(unknown), lazy=False)


@pytest.fixture
def io_failure_dataset(tmp_path, gdal):
    path = tmp_path / "io-errors.tif"
    dataset = gdal.GetDriverByName("GTiff").Create(str(path), 2, 2, 1, gdal.GDT_Byte)
    dataset.SetGeoTransform([0, 1, 0, 2, 0, -1])
    dataset.GetRasterBand(1).Fill(11)
    dataset.CreateMaskBand(gdal.GMF_PER_DATASET)
    dataset.GetRasterBand(1).GetMaskBand().Fill(255)
    dataset = None
    return path


@pytest.mark.parametrize("surface", ["open", "integer", "sampled", "resampled", "mask", "update"])
@pytest.mark.parametrize(
    "error_type", [MemoryError, terrio.ResourceLimitError], ids=["allocation", "policy"]
)
def test_rio_distinguishes_policy_refusals_from_other_memory_errors(
    io_failure_dataset, monkeypatch, surface, error_type
):
    """Policy errors keep their identity; unrelated allocation errors keep I/O wrapping."""
    import terrio.rio as rio
    from terrio.rio import io
    from terrio.rio.errors import RasterioIOError

    injected = error_type("injected allocation refusal")

    def fail(*args, **kwargs):
        raise injected

    if surface == "open":
        monkeypatch.setattr(io._terrio, "open_raster", fail)
    else:
        method = {
            "integer": "read",
            "sampled": "read_sampled",
            "resampled": "read_resampled",
            "mask": "read_resampled_masks",
            "update": "read",
        }[surface]
        monkeypatch.setattr(io._terrio.Raster, method, fail)
        if surface == "sampled":
            # Exercise the fallback used by reads whose level/format bypasses core resampling.
            monkeypatch.setattr(io.DatasetReader, "_core_resampled", lambda *args: None)

    expected = error_type if error_type is terrio.ResourceLimitError else RasterioIOError
    with pytest.raises(expected) as caught:
        with rio.open(io_failure_dataset, "r+" if surface == "update" else "r") as dataset:
            if surface == "integer":
                dataset.read()
            elif surface == "sampled":
                dataset.read(out_shape=(1, 1, 1))
            elif surface == "resampled":
                dataset.read(out_shape=(1, 4, 4))
            elif surface == "mask":
                dataset.read_masks(out_shape=(1, 4, 4))
    if error_type is terrio.ResourceLimitError:
        assert caught.value is injected
    else:
        assert caught.value.__cause__ is injected


@pytest.mark.parametrize("surface", ["construct", "whole", "streaming"])
@pytest.mark.parametrize(
    "error_type", [MemoryError, terrio.ResourceLimitError], ids=["allocation", "policy"]
)
def test_rio_writer_preserves_policy_errors_and_cleans_temporary_files(
    tmp_path, monkeypatch, surface, error_type
):
    """Both writer paths preserve policy refusals without leaving temporary outputs."""
    import numpy as np

    import terrio.rio as rio
    from terrio.rio import io
    from terrio.rio.errors import RasterioIOError

    path = tmp_path / "output.tif"
    injected = error_type("injected writer refusal")

    def fail(*args, **kwargs):
        if surface != "construct":
            target = args[1]
            with open(target, "wb") as stream:
                stream.write(b"incomplete")
        raise injected

    if surface == "construct":
        monkeypatch.setattr(io._terrio.Raster, "from_array", fail)
    elif surface == "whole":
        monkeypatch.setattr(io._terrio.Raster, "write", fail)
    else:
        monkeypatch.setattr(io.DatasetWriter, "_write_streaming", fail)

    expected = error_type if error_type is terrio.ResourceLimitError else RasterioIOError
    with pytest.raises(expected) as caught:
        with rio.open(
            path, "w", driver="GTiff", width=2, height=2, count=1, dtype="uint8"
        ) as dataset:
            dataset.write(np.ones((1, 2, 2), dtype="uint8"))
            # Use the transactional update target for whole writes as well as streamed ones.
            # The injection exercises cleanup after the output has actually been created.
            dataset.mode = "r+"
            if surface == "streaming":
                monkeypatch.setattr(dataset, "_streams_on_flush", lambda: True)
    if error_type is terrio.ResourceLimitError:
        assert caught.value is injected
    else:
        assert caught.value.__cause__ is injected
    assert dataset.closed
    assert list(tmp_path.iterdir()) == []


@pytest.mark.skipif(sys.platform != "linux", reason="uses the Linux usable-memory policy")
def test_vrt_working_memory_refusal_reaches_python(tmp_path):
    """A small Byte window fits while full-size Float64 VRT work does not."""
    path = tmp_path / "working.vrt"
    path.write_text(
        '<VRTDataset rasterXSize="3000" rasterYSize="3000">'
        '<VRTRasterBand dataType="Byte" band="1"><NoDataValue>7</NoDataValue>'
        "</VRTRasterBand></VRTDataset>"
    )
    script = """
import resource
import sys
import terrio
soft, hard = resource.getrlimit(resource.RLIMIT_RSS)
resource.setrlimit(resource.RLIMIT_RSS, (min(hard, 128 << 20) if hard >= 0 else 128 << 20, hard))
raster = terrio.open_raster(sys.argv[1], lazy=True)
small = raster.read(1, window=(0, 0, 16, 16))
assert small.dtype.name == "uint8" and (small == 7).all()
try:
    raster.read()
except terrio.ResourceLimitError as error:
    assert "VRT working set" in str(error), str(error)
else:
    raise AssertionError("VRT working arrays exceeded the usable-memory policy")
"""
    environment = dict(os.environ)
    for option in ("TERRIO_MAX_DECLARED_BYTES", "TERRIO_MAX_BANDS"):
        environment.pop(option, None)
    result = subprocess.run(
        [sys.executable, "-c", script, str(path)],
        env=environment,
        capture_output=True,
        text=True,
        timeout=60,
    )
    assert result.returncode == 0, result.stdout + result.stderr


@pytest.fixture(scope="module")
def large_uint16_alpha(tmp_path_factory, gdal):
    """GDAL writes separate 32 MiB alpha/data strips, each individually within the policy."""
    path = tmp_path_factory.mktemp("alpha-memory") / "alpha.tif"
    dataset = gdal.GetDriverByName("GTiff").Create(
        str(path),
        4096,
        4096,
        2,
        gdal.GDT_UInt16,
        options=["COMPRESS=DEFLATE", "INTERLEAVE=BAND", "BLOCKYSIZE=4096"],
    )
    dataset.SetGeoTransform([0, 1, 0, 4096, 0, -1])
    dataset.GetRasterBand(2).SetColorInterpretation(gdal.GCI_AlphaBand)
    dataset.GetRasterBand(1).Fill(17)
    dataset.GetRasterBand(2).Fill(65535)
    dataset = None
    reference = gdal.Open(str(path))
    assert reference.GetRasterBand(2).GetBlockSize() == [4096, 4096]
    assert reference.GetRasterBand(1).GetMaskFlags() & gdal.GMF_ALPHA
    assert reference.GetRasterBand(1).GetMaskBand().ReadAsArray(0, 0, 16, 16).min() == 255
    return path


@pytest.mark.skipif(sys.platform != "linux", reason="uses the Linux usable-memory policy")
@pytest.mark.parametrize("method", ["nearest", "bilinear", "average"])
def test_resized_alpha_source_memory_refusal(large_uint16_alpha, method):
    """A small mask output cannot conceal the source window's UInt16/Byte conversion peak."""
    script = """
import resource
import sys
import terrio
_, hard = resource.getrlimit(resource.RLIMIT_RSS)
resource.setrlimit(resource.RLIMIT_RSS, (min(hard, 32 << 20) if hard >= 0 else 32 << 20, hard))
path, method = sys.argv[1:]
dataset = terrio.open_raster(path, lazy=True)
small = dataset.read_resampled_masks((8, 8), band=1, window=(0, 0, 16, 16), resampling=method)
assert small.shape == (8, 8) and (small == 255).all()
try:
    dataset.read_resampled_masks((512, 512), band=1, resampling=method)
except terrio.ResourceLimitError as error:
    assert "alpha mask source/conversion working set" in str(error), str(error)
else:
    raise AssertionError("unchecked UInt16 alpha conversion")
"""
    environment = dict(os.environ)
    for option in ("TERRIO_MAX_DECLARED_BYTES", "TERRIO_MAX_BANDS", "TERRIO_TIFF_MAX_CHUNK_BYTES"):
        environment.pop(option, None)
    result = subprocess.run(
        [sys.executable, "-c", script, str(large_uint16_alpha), method],
        env=environment,
        capture_output=True,
        text=True,
        timeout=60,
    )
    assert result.returncode == 0, result.stdout + result.stderr
