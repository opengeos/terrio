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
