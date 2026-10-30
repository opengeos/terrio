"""Dataset file sets through the public API, including real GDAL sidecars."""

import numpy as np
import pytest

import terrio


def test_copy_rename_delete_paths_and_batch_preflight(tmp_path):
    source = tmp_path / "source.csv"
    source.write_text("x,y,name\n1,2,first\n")
    source.with_suffix(".csvt").write_text("Real,Real,String")
    source.with_suffix(".txt").write_text("keep")
    target = tmp_path / "copy.csv"
    files = terrio.dataset_files(source)
    assert files["driver"] == "CSV"
    assert set(files["files"]) == {str(source), str(source.with_suffix(".csvt"))}
    report = terrio.dataset_copy(source, target, format="CSV")
    assert report["bytes"] == source.stat().st_size + source.with_suffix(".csvt").stat().st_size
    assert target.read_bytes() == source.read_bytes()
    with pytest.raises(ValueError, match="exists"):
        terrio.dataset_copy(source, target)
    moved = tmp_path / "moved.csv"
    assert terrio.dataset_rename(target, moved)["bytes"] == 0
    assert moved.with_suffix(".csvt").read_text() == "Real,Real,String"
    assert not target.exists()
    with pytest.raises(ValueError, match="does not exist"):
        terrio.dataset_delete([moved, tmp_path / "missing.csv"])
    assert moved.exists()
    reports = terrio.dataset_delete(p for p in [source, moved])
    assert len(reports) == 2
    assert sorted(p.name for p in tmp_path.iterdir()) == ["source.txt"]


def test_real_tiff_overviews_masks_and_metadata_remain_readable(tmp_path):
    gdal = pytest.importorskip("osgeo.gdal")
    gdal.UseExceptions()
    source = tmp_path / "source.tif"
    dataset = gdal.GetDriverByName("GTiff").Create(str(source), 32, 32, 1, gdal.GDT_Int16)
    dataset.SetGeoTransform((10, 2, 0, 100, 0, -2))
    dataset.SetProjection("EPSG:4326")
    band = dataset.GetRasterBand(1)
    band.SetNoDataValue(-9999)
    values = np.arange(1024, dtype="int16").reshape(32, 32)
    band.WriteArray(values)
    dataset = band = None
    dataset = gdal.Open(str(source), gdal.GA_ReadOnly)
    dataset.BuildOverviews("NEAREST", [2, 4])
    dataset.GetRasterBand(1).SetStatistics(0, 1023, 511.5, 295.6)
    dataset = None
    # The external mask format can be copied independently of TIFF internal masks.
    mask = gdal.GetDriverByName("GTiff").Create(str(source) + ".msk", 32, 32, 1, gdal.GDT_Byte)
    mask.SetMetadataItem("INTERNAL_MASK_FLAGS_1", "2")
    mask.GetRasterBand(1).Fill(255)
    mask.GetRasterBand(1).WriteArray(np.zeros((1, 32), dtype="uint8"), 0, 0)
    mask = None
    source.with_suffix(".tfw").write_text("1\n0\n0\n-1\n0\n0\n")
    target = tmp_path / "copy.tif"
    report = terrio.dataset_copy(source, target)
    assert len(report["files"]) == 4
    assert not target.with_suffix(".tfw").exists()  # Embedded transform takes precedence.
    result = gdal.Open(str(target))
    np.testing.assert_array_equal(result.ReadAsArray(), values)
    assert result.GetGeoTransform() == (10, 2, 0, 100, 0, -2)
    assert result.GetRasterBand(1).GetOverviewCount() == 2
    assert result.GetRasterBand(1).GetNoDataValue() == -9999
    np.testing.assert_array_equal(result.GetRasterBand(1).GetMaskBand().ReadAsArray()[0], 0)
    result = None
    terrio.dataset_delete(target)
    assert not list(tmp_path.glob("copy*"))
    assert source.with_suffix(".tfw").exists()


def test_vsimem_and_overwrite_remove_obsolete_sidecars():
    root = "/vsimem/python-dataset-files"
    source = root + "/source.csv"
    target = root + "/target.csv"
    terrio.vsi_write(source, b"x,y\n1,2\n")
    terrio.vsi_write(target, b"x,y\n3,4\n")
    terrio.vsi_write(root + "/target.csvt", b"Real,Real")
    try:
        terrio.dataset_copy(source, target, overwrite=True)
        assert terrio.vsi_stat(root + "/target.csvt") is None
        assert terrio.vsi_read(target) == b"x,y\n1,2\n"
        with pytest.raises(ValueError, match="overlap"):
            terrio.dataset_copy(source, source, overwrite=True)
    finally:
        terrio.vsi_delete(root, recursive=True)


def test_vrt_reference_scope_and_explicit_external_opt_in(tmp_path):
    inputs = tmp_path / "inputs"
    inputs.mkdir()
    raw = tmp_path / "source.raw"
    raw.write_bytes(bytes([1, 2, 3, 4]))
    source = inputs / "source.vrt"
    source.write_text(
        '<VRTDataset rasterXSize="2" rasterYSize="2">'
        '<VRTRasterBand band="1" dataType="Byte" subClass="VRTRawRasterBand">'
        '<SourceFilename relativeToVRT="1">../source.raw</SourceFilename>'
        "<ImageOffset>0</ImageOffset><PixelOffset>1</PixelOffset><LineOffset>2</LineOffset>"
        "</VRTRasterBand></VRTDataset>"
    )
    output = tmp_path / "copy.vrt"
    with pytest.raises(Exception, match="source directory tree"):
        terrio.dataset_copy(source, output)
    assert not output.exists()
    report = terrio.dataset_copy(source, output, allow_external_references=True)
    assert report["recovery_files"] == []
    assert output.with_suffix(".raw").read_bytes() == raw.read_bytes()
    assert raw.exists()


@pytest.mark.parametrize("separator", ["%2f", "%2F"])
def test_remote_vrt_encoded_separator_is_refused_before_fetch(tmp_path, separator):
    from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
    from threading import Thread

    gdal = pytest.importorskip("osgeo.gdal")
    gdal.UseExceptions()
    payload_path = tmp_path / "outside.tif"
    dataset = gdal.GetDriverByName("GTiff").Create(str(payload_path), 2, 2, 1, gdal.GDT_Byte)
    dataset.GetRasterBand(1).Fill(7)
    dataset = None
    payload = payload_path.read_bytes()
    requests = []
    secret_path = f"/work{separator}secret/source.tif"

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def serve(self, body):
            requests.append(self.path)
            if self.path == "/work/source.vrt":
                content = vrt
            elif self.path.lower() == secret_path.lower():
                content = payload
            else:
                self.send_error(404)
                return
            self.send_response(200)
            self.send_header("Content-Length", str(len(content)))
            self.end_headers()
            if body:
                self.wfile.write(content)

        def do_HEAD(self):
            self.serve(False)

        def do_GET(self):
            self.serve(True)

    with ThreadingHTTPServer(("127.0.0.1", 0), Handler) as server:
        base = f"http://127.0.0.1:{server.server_port}"
        vrt = (
            '<VRTDataset rasterXSize="2" rasterYSize="2">'
            '<VRTRasterBand dataType="Byte" band="1"><SimpleSource>'
            f'<SourceFilename relativeToVRT="0">{base}{secret_path}</SourceFilename>'
            "<SourceBand>1</SourceBand></SimpleSource></VRTRasterBand></VRTDataset>"
        ).encode()
        worker = Thread(target=server.serve_forever, daemon=True)
        worker.start()
        output = tmp_path / "copy.vrt"
        try:
            with pytest.raises(ValueError, match="source directory tree"):
                terrio.dataset_copy(f"{base}/work/source.vrt", output)
            assert not any(
                path.lower().startswith(f"/work{separator}secret".lower()) for path in requests
            )
            assert not output.exists()
            assert not output.with_suffix(".tif").exists()
            terrio.dataset_copy(f"{base}/work/source.vrt", output, allow_external_references=True)
            assert any(path.lower() == secret_path.lower() for path in requests)
            result = gdal.Open(str(output))
            np.testing.assert_array_equal(result.ReadAsArray(), np.full((2, 2), 7, dtype="uint8"))
            result = None
        finally:
            server.shutdown()
            worker.join()
