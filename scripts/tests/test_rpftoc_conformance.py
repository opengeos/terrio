"""The RPF cross-read verifier must reject changed raster properties and payloads."""

import sys
import subprocess
import time
from unittest.mock import patch
from pathlib import Path

import numpy as np
from osgeo import gdal, osr
import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from compare_rpftoc_with_gdal import compare, mcp_report
from make_rpftoc_fixtures import verify_polar


def indexed_raster(root, name, change=None):
    raster = root / f"{name}.tif"
    ds = gdal.GetDriverByName("GTiff").Create(str(raster), 2, 2, 1, gdal.GDT_Byte)
    ds.SetGeoTransform((0.00005 if change == "grid" else 0, 1, 0, 2, 0, -1))
    crs = osr.SpatialReference()
    crs.ImportFromEPSG(4326 if change == "crs" else 32611)
    ds.SetSpatialRef(crs)
    band = ds.GetRasterBand(1)
    pixels = np.array([[0, 1], [1, 0]], dtype="uint8")
    if change == "pixels":
        pixels[0, 0] = 1
    band.SetNoDataValue(0 if change == "nodata" else 255)
    palette = gdal.ColorTable()
    palette.SetColorEntry(0, (0, 0, 0, 255))
    palette.SetColorEntry(
        1, (0, 0, 255, 255) if change == "palette" else (255, 255, 255, 255)
    )
    band.SetColorTable(palette)
    band.WriteArray(pixels)
    band.CreateMaskBand(gdal.GMF_PER_DATASET)
    mask = np.full((2, 2), 255, dtype="uint8")
    if change == "mask":
        mask[0, 0] = 0
    band.GetMaskBand().WriteArray(mask)
    ds.Close()
    index = root / f"{name}.vrt"
    ds = gdal.GetDriverByName("VRT").Create(str(index), 1, 1, 1, gdal.GDT_Byte)
    ds.SetMetadata(
        {"SUBDATASET_1_NAME": str(raster), "SUBDATASET_1_DESC": "fixture"},
        "SUBDATASETS",
    )
    ds.Close()
    return index


def test_matching_payload_and_metadata_are_identical(tmp_path):
    assert (
        compare(indexed_raster(tmp_path, "a"), indexed_raster(tmp_path, "b"))[0]
        == "identical"
    )


@pytest.mark.parametrize(
    "change", ["pixels", "mask", "palette", "grid", "crs", "nodata"]
)
def test_changed_raster_is_rejected(tmp_path, change):
    with pytest.raises(AssertionError):
        compare(
            indexed_raster(tmp_path, "actual", change),
            indexed_raster(tmp_path, "reference"),
        )


def test_polar_download_checksum_rejects_other_nitf_payloads(tmp_path):
    path = tmp_path / "fake.ON9"
    path.write_bytes(b"NITF02.00" + bytes(100))
    with pytest.raises(RuntimeError, match="checksum"):
        verify_polar(path)


def test_mcp_reply_deadline_terminates_a_stalled_server():
    original_popen = subprocess.Popen
    processes = []

    def stalled_server(_arguments, **kwargs):
        process = original_popen(
            [sys.executable, "-c", "import time; time.sleep(60)"], **kwargs
        )
        processes.append(process)
        return process

    start = time.monotonic()
    with patch("compare_rpftoc_with_gdal.subprocess.Popen", side_effect=stalled_server):
        with pytest.raises(AssertionError, match="MCP replies timed out"):
            mcp_report(Path("unused"), [], timeout=0.05)
    assert processes[0].poll() is not None
    assert time.monotonic() - start < 4


def test_partial_generated_fixture_is_repaired_without_replacing_existing_files(
    tmp_path,
):
    from make_rpftoc_fixtures import install_frame_inventory

    generated, target = tmp_path / "generated", tmp_path / "target"
    generated.mkdir()
    target.mkdir()
    for name in ["00000000.ON2", "00000001.ON2"]:
        (generated / name).write_bytes(name.encode())
    (target / "00000000.ON2").write_bytes(b"keep existing frame")
    install_frame_inventory(target, generated, "*.ON2")
    assert (target / "00000000.ON2").read_bytes() == b"keep existing frame"
    assert (target / "00000001.ON2").read_bytes() == b"00000001.ON2"
    (target / "00000002.ON2").write_bytes(b"unexpected")
    with pytest.raises(RuntimeError, match="unexpected RPF fixture inventory"):
        install_frame_inventory(target, generated, "*.ON2")


def test_missing_rpf_reference_capability_skips_suite(monkeypatch, tmp_path):
    import conformance
    import sys
    from types import SimpleNamespace
    from osgeo import gdal

    monkeypatch.setitem(sys.modules, "terrio", SimpleNamespace(rpftoc_bytes=True))
    monkeypatch.setattr(conformance, "missing_modules", lambda *args: [])
    monkeypatch.setattr(gdal, "VersionInfo", lambda *args: "3130300")
    monkeypatch.setattr(gdal, "GetDriverByName", lambda *args: None)
    suite = conformance.suite_rpftoc(SimpleNamespace(data=tmp_path))
    assert "NITF/RPFTOC" in suite.skipped and not suite.cases


def test_optional_node_probe_checks_wasm_and_exports(tmp_path):
    import shutil
    from conformance import node_exports_available

    if shutil.which("node") is None:
        pytest.skip("Node is unavailable")
    module = tmp_path / "terrio.js"
    module.write_text("module.exports={rpftocCreate(){}}")
    assert not node_exports_available(module, ["rpftocCreate"])
    module.with_name("terrio_bg.wasm").touch()
    assert node_exports_available(module, ["rpftocCreate"])
    assert not node_exports_available(module, ["missingExport"])


def test_missing_node_reports_not_compared_without_losing_other_results(monkeypatch, tmp_path):
    import json
    import conformance
    from types import SimpleNamespace
    from osgeo import gdal

    monkeypatch.setitem(sys.modules, "terrio", SimpleNamespace(rpftoc_bytes=True))
    monkeypatch.setattr(conformance, "missing_modules", lambda *args: [])
    monkeypatch.setattr(gdal, "VersionInfo", lambda *args: "3130300")
    monkeypatch.setattr(gdal, "GetDriverByName", lambda *args: object())
    algorithm = SimpleNamespace(GetSubAlgorithmNames=lambda: ["rpftoc"])
    monkeypatch.setattr(gdal, "GetGlobalAlgorithmRegistry", lambda: SimpleNamespace(InstantiateAlg=lambda name: algorithm))
    for directory in ["rpftoc_landsat", "rpftoc_polar", "rpftoc_fractional3500000", "rpftoc_pole_centers/north-rgb", "rpftoc_pole_centers/south-rgb"]:
        (tmp_path / directory).mkdir(parents=True)
    monkeypatch.setattr(conformance, "node_exports_available", lambda *args: False)
    results = [dict(name=str(i), status="identical", detail="reference matches") for i in range(20)]
    monkeypatch.setattr(conformance, "run", lambda args: (0, json.dumps(results), ""))
    suite = conformance.suite_rpftoc(SimpleNamespace(data=str(tmp_path), terrio="terrio"))
    assert not suite.error and not suite.skipped
    assert len(suite.cases) == 21
    assert suite.cases[0].status == conformance.NOT_COMPARED
    assert all(case.status == conformance.IDENTICAL for case in suite.cases[1:])


def test_failed_fixture_copy_never_publishes_a_partial_frame(monkeypatch, tmp_path):
    import shutil
    from make_rpftoc_fixtures import install_frame_inventory

    generated, target = tmp_path / "generated", tmp_path / "target"
    generated.mkdir()
    (generated / "00000000.ON2").write_bytes(b"complete frame")

    def interrupted_copy(source, output):
        output.write(b"partial")
        raise OSError("interrupted fixture copy")

    with monkeypatch.context() as patcher:
        patcher.setattr(shutil, "copyfileobj", interrupted_copy)
        with pytest.raises(OSError, match="interrupted fixture copy"):
            install_frame_inventory(target, generated, "*.ON2")
    assert not list(target.iterdir())
    install_frame_inventory(target, generated, "*.ON2")
    assert (target / "00000000.ON2").read_bytes() == b"complete frame"


def test_fixture_copy_preserves_default_umask_based_file_mode(tmp_path):
    import stat
    from make_rpftoc_fixtures import install_frame_inventory

    generated, target = tmp_path / "generated", tmp_path / "target"
    generated.mkdir()
    reference = tmp_path / "reference"
    reference.write_bytes(b"reference mode")
    (generated / "00000000.ON2").write_bytes(b"frame")
    install_frame_inventory(target, generated, "*.ON2")
    assert stat.S_IMODE((target / "00000000.ON2").stat().st_mode) == stat.S_IMODE(reference.stat().st_mode)


def test_fixture_temporary_collision_does_not_remove_an_unowned_file(monkeypatch, tmp_path):
    import make_rpftoc_fixtures as fixtures
    from types import SimpleNamespace

    generated, target = tmp_path / "generated", tmp_path / "target"
    generated.mkdir()
    target.mkdir()
    (generated / "00000000.ON2").write_bytes(b"frame")
    existing = target / ".00000000.ON2.fixed.partial"
    existing.write_bytes(b"preserve someone else's file")
    monkeypatch.setattr(fixtures.uuid, "uuid4", lambda: SimpleNamespace(hex="fixed"))
    with pytest.raises(FileExistsError):
        fixtures.install_frame_inventory(target, generated, "*.ON2")
    assert existing.read_bytes() == b"preserve someone else's file"
    assert not (target / "00000000.ON2").exists()
