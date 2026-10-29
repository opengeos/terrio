"""Dataset read checks, including corrupt content with a recognizable header."""

import numpy as np
import pytest

import terrio


@pytest.fixture
def raster(tmp_path):
    path = tmp_path / "source.tif"
    source = terrio.Raster.from_array(
        np.arange(24, dtype="uint8").reshape(2, 3, 4), (0, 1, 0, 3, 0, -1), "EPSG:4326"
    )
    source.write(str(path))
    return path


def test_path_and_upload_check_counts(raster):
    summary = terrio.check_dataset(raster)
    assert summary["raster_samples"] == 24
    assert summary["raster_windows"] == 1
    assert summary["features"] == 0
    assert terrio.check_dataset(raster.read_bytes(), filename="source.tif") == summary
    assert terrio.check_dataset(raster, overviews=True, masks=True) == summary
    with pytest.raises(ValueError, match="filename"):
        terrio.check_dataset(raster, filename="unused.tif")
    with pytest.raises(ValueError, match="expects a path"):
        terrio.check_dataset([raster])


def test_vector_reads_and_decode_errors(tmp_path):
    source = tmp_path / "points.geojson"
    source.write_text(
        '{"type":"FeatureCollection","features":['
        '{"type":"Feature","properties":{},"geometry":{"type":"Point","coordinates":[0,0]}}]}'
    )
    assert terrio.check_dataset(source)["features"] == 1
    assert terrio.check_dataset(source.read_bytes(), filename="points.geojson")["layers"] == 1
    source.write_bytes(source.read_bytes()[:-8])
    with pytest.raises(ValueError):
        terrio.check_dataset(source)
    with pytest.raises(ValueError):
        terrio.check_dataset(b"II*\0broken", filename="broken.tif")


def test_recognizable_tiff_with_damaged_payload_fails(tmp_path):
    gdal = pytest.importorskip("osgeo.gdal")
    gdal.UseExceptions()
    path = tmp_path / "compressed.tif"
    dataset = gdal.GetDriverByName("GTiff").Create(
        str(path),
        260,
        260,
        1,
        gdal.GDT_Byte,
        options=["TILED=YES", "BLOCKXSIZE=128", "BLOCKYSIZE=128", "COMPRESS=DEFLATE"],
    )
    dataset.GetRasterBand(1).WriteArray(np.arange(260 * 260, dtype="uint8").reshape(260, 260))
    dataset.FlushCache()
    band = dataset.GetRasterBand(1)
    offset = int(band.GetMetadataItem("BLOCK_OFFSET_2_2", "TIFF"))
    length = int(band.GetMetadataItem("BLOCK_SIZE_2_2", "TIFF"))
    band = dataset = None
    with path.open("r+b") as stream:
        stream.seek(offset)
        stream.write(bytes(length))
    assert terrio.identify(path)[0]["driver"] == "GTiff"
    with pytest.raises(ValueError):
        terrio.check_dataset(path)


def test_all_requested_overviews_and_dataset_masks(tmp_path):
    gdal = pytest.importorskip("osgeo.gdal")
    gdal.UseExceptions()
    path = tmp_path / "pyramid.tif"
    with gdal.config_option("GDAL_TIFF_INTERNAL_MASK", "YES"):
        dataset = gdal.GetDriverByName("GTiff").Create(str(path), 4, 4, 2, gdal.GDT_Byte)
        for number in [1, 2]:
            dataset.GetRasterBand(number).WriteArray(np.full((4, 4), number, dtype="uint8"))
        dataset.CreateMaskBand(gdal.GMF_PER_DATASET)
        dataset.GetRasterBand(1).GetMaskBand().WriteArray(np.full((4, 4), 255, dtype="uint8"))
        dataset.BuildOverviews("NEAREST", [2])
        dataset = None
    assert terrio.check_dataset(path)["raster_samples"] == 32
    summary = terrio.check_dataset(path, overviews=True, masks=True)
    assert summary["raster_samples"] == 40
    assert summary["mask_samples"] == 20


def test_scientific_handle_and_userblock_uploads(tmp_path):
    import shutil
    import subprocess

    gdal = pytest.importorskip("osgeo.gdal")
    if shutil.which("h5repack") is None:
        pytest.skip("h5repack is not available")
    gdal.UseExceptions()
    source = tmp_path / "source.nc"
    path = tmp_path / "arrays.h5"

    def create():
        dataset = gdal.GetDriverByName("netCDF").CreateMultiDimensional(
            str(source), options=["FORMAT=NC4"]
        )
        root = dataset.GetRootGroup()
        dimensions = []
        for name, length in [("x", 2), ("y", 3)]:
            dimension = root.CreateDimension(name, None, None, length)
            coordinate = root.CreateMDArray(
                name, [dimension], gdal.ExtendedDataType.Create(gdal.GDT_Float64)
            )
            coordinate.Write(np.arange(length, dtype="<f8").tobytes())
            dimensions.append(dimension)
        root.CreateMDArray(
            "grid", dimensions, gdal.ExtendedDataType.Create(gdal.GDT_Float32)
        ).Write(np.arange(6, dtype="<f4").tobytes())
        root.CreateMDArray("scalar", [], gdal.ExtendedDataType.Create(gdal.GDT_Int32)).Write(
            np.asarray(7, dtype="<i4").tobytes()
        )
        dataset.FlushCache()

    create()
    subprocess.run(["h5repack", "-b", "16384", str(source), str(path)], check=True, timeout=30)
    # Identification deliberately stops before this HDF5 signature; a content
    # check must still discover and read every scientific array.
    assert terrio.identify(path) == []
    summary = terrio.check_dataset(path)
    assert summary["arrays"] == 4
    assert summary["array_elements"] == 12
    assert terrio.check_dataset(path.read_bytes(), filename="unhelpful.bin") == summary
    assert terrio.open_multidim(path).check() == summary


def test_real_point_cloud_paths_and_uploads():
    import os
    import struct
    from pathlib import Path

    directory = os.environ.get("TERRIO_TEST_DATA")
    if directory is None:
        pytest.skip("TERRIO_TEST_DATA is not set")
    paths = [
        Path(directory) / "pointcloud" / f"autzen_trim.{extension}" for extension in ["las", "laz"]
    ]
    if not all(path.is_file() for path in paths):
        pytest.skip("Autzen point-cloud fixtures are not available")
    for path in paths:
        data = path.read_bytes()
        expected = struct.unpack_from("<I", data, 107)[0]
        if data[25] >= 4 and expected == 0:
            expected = struct.unpack_from("<Q", data, 247)[0]
        assert expected > 0
        assert terrio.check_dataset(path)["points"] == expected
        assert terrio.check_dataset(data, filename=path.name)["points"] == expected
