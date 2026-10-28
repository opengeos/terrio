"""Palette conversion across in-memory, serialized and guarded path APIs."""

import numpy as np
import pytest

import terrio


def test_palette_roundtrip_and_destination_guards(tmp_path):
    values = np.array([[[248, 0]], [[0, 248]], [[0, 0]]], dtype="uint8")
    source = terrio.Raster.from_array(values, (10, 1, 0, 20, 0, -1), crs="EPSG:4326")
    colors = [[248, 0, 0, 255], [0, 248, 0, 255]]
    path = str(tmp_path / "indexed.tif")
    indexed = terrio.rgb_to_palette(source, path, color_map=colors, dither=False, bit_depth=8)
    np.testing.assert_array_equal(indexed.read(1), [[0, 1]])
    expanded = terrio.palette_to_rgb(path)
    for b in range(1, 4):
        np.testing.assert_array_equal(expanded.read(b), values[b - 1])
    attached = terrio.attach_palette(indexed, [[1, 2, 3, 255], [4, 5, 6, 255]])
    np.testing.assert_array_equal(attached.read(1), indexed.read(1))
    assert terrio.read_color_table(attached)[0] == [1, 2, 3, 255]
    before = (tmp_path / "indexed.tif").read_bytes()
    opened = terrio.open_raster(path, lazy=True)
    with pytest.raises(ValueError, match="input"):
        terrio.palette_to_rgb(opened, path, overwrite=True)
    with pytest.raises(ValueError, match="input"):
        terrio.rgb_to_palette(source, path, color_map=opened, overwrite=True)
    with pytest.raises(ValueError, match="exists"):
        terrio.rgb_to_palette(source, path)
    with pytest.raises(ValueError, match="input"):
        terrio.palette_to_rgb(path, path, overwrite=True)
    alias = tmp_path / "alias.tif"
    alias.hardlink_to(tmp_path / "indexed.tif")
    with pytest.raises(ValueError, match="alias"):
        terrio.attach_palette(path, colors, str(alias), overwrite=True)
    assert (tmp_path / "indexed.tif").read_bytes() == before
    with pytest.raises(ValueError, match="dimensions"):
        terrio.rgb_to_palette(source, mask=np.ones((2, 1), dtype="uint8"))
    with pytest.raises(ValueError, match="bit depth"):
        terrio.rgb_to_palette(source, bit_depth=6)


def test_palette_mask_and_color_files():
    source = terrio.Raster.from_array(np.full((3, 2, 2), 248, dtype="uint8"), (0, 1, 0, 2, 0, -1))
    out = terrio.rgb_to_palette(source, mask=np.array([[0, 255], [255, 255]], dtype="uint8"))
    assert out.nodata == 0
    assert out.read(1)[0, 0] == 0
    rgba = terrio.palette_to_rgb(out, rgba=True)
    assert rgba.read(4)[0, 0] == 0
    assert rgba.read(4)[1, 1] == 255
    assert terrio.read_color_table(b"0 1 2 3 255\n2 red 128")[2] == [255, 0, 0, 128]


def test_palette_dataset_mask_survives_serialization(tmp_path):
    gdal = pytest.importorskip("osgeo.gdal")
    gdal.UseExceptions()
    path = str(tmp_path / "masked.tif")
    ds = gdal.GetDriverByName("GTiff").Create(path, 2, 2, 1, gdal.GDT_Byte)
    ds.SetGeoTransform((10, 1, 0, 20, 0, -1))
    table = gdal.ColorTable()
    table.SetColorEntry(0, (10, 20, 30, 255))
    table.SetColorEntry(1, (40, 50, 60, 255))
    ds.GetRasterBand(1).SetColorTable(table)
    ds.GetRasterBand(1).WriteArray(np.array([[0, 1], [1, 0]], dtype="uint8"))
    ds.CreateMaskBand(gdal.GMF_PER_DATASET)
    mask = np.array([[0, 255], [255, 255]], dtype="uint8")
    ds.GetRasterBand(1).GetMaskBand().WriteArray(mask)
    ds = None
    for result in [terrio.palette_to_rgb(path), terrio.attach_palette(path, [[1, 2, 3, 255]])]:
        output = str(tmp_path / "output.tif")
        result.write(output)
        actual = gdal.Open(output)
        np.testing.assert_array_equal(actual.GetRasterBand(1).GetMaskBand().ReadAsArray(), mask)
        actual = None
        memory_path = "/vsimem/terrio-palette-test.tif"
        gdal.FileFromMemBuffer(memory_path, result.to_bytes("output.tif"))
        try:
            actual = gdal.Open(memory_path)
            np.testing.assert_array_equal(actual.GetRasterBand(1).GetMaskBand().ReadAsArray(), mask)
            actual = None
        finally:
            gdal.Unlink(memory_path)


def test_remote_mask_write_io_and_tile_archive_refusal(tmp_path, gdal):
    """Measure image/mask bytes separately and guard the streaming archive path."""
    import functools
    import http.server
    import threading
    from pathlib import Path

    class Handler(http.server.SimpleHTTPRequestHandler):
        sent = 0
        requests = 0
        paths = set()

        def log_message(self, *_):
            pass

        def send_head(self):
            path = Path(self.translate_path(self.path))
            if not path.is_file():
                self.send_error(404)
                return None
            size = path.stat().st_size
            start, end = 0, size - 1
            span = self.headers.get("Range")
            if span:
                first, last = span.removeprefix("bytes=").split("-", 1)
                start, end = int(first), min(int(last) if last else end, end)
                self.send_response(206)
                self.send_header("Content-Range", f"bytes {start}-{end}/{size}")
            else:
                self.send_response(200)
            self.send_header("Content-Length", str(end - start + 1))
            self.send_header("Accept-Ranges", "bytes")
            self.end_headers()
            self.length = end - start + 1
            stream = path.open("rb")
            stream.seek(start)
            return stream

        def copyfile(self, source, output):
            payload = source.read(self.length)
            type(self).sent += len(payload)
            type(self).requests += 1
            type(self).paths.add(self.path.split("?")[0])
            output.write(payload)

    values = np.resize(np.arange(251, dtype="uint8"), (3, 512, 512))
    mask = np.full((512, 512), 255, dtype="uint8")
    mask[:256, :256] = 0
    local = str(tmp_path / "masked-cog.tif")
    terrio.Raster.from_array(values, (0, 1, 0, 512, 0, -1), crs="EPSG:3857").write(
        local, driver="COG", tile_size=256, mask=mask, compress="NONE"
    )
    server = http.server.ThreadingHTTPServer(
        ("127.0.0.1", 0), functools.partial(Handler, directory=str(tmp_path))
    )
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    observations = []
    try:
        for mode in ["pixels", "write"]:
            Handler.sent = Handler.requests = 0
            Handler.paths = set()
            raster = terrio.open_raster(
                f"http://127.0.0.1:{server.server_port}/masked-cog.tif?mode={mode}", lazy=True
            )
            if mode == "pixels":
                np.testing.assert_array_equal(raster.read(), values)
            else:
                path = str(tmp_path / "output.tif")
                raster.write(path)
                result = gdal.Open(path)
                np.testing.assert_array_equal(result.ReadAsArray(), values)
                np.testing.assert_array_equal(
                    result.GetRasterBand(1).GetMaskBand().ReadAsArray(), mask
                )
                result = None
            observations.append((Handler.sent, Handler.requests, len(Handler.paths)))
        assert observations[1][0] >= observations[0][0]
        assert observations[0][2] == observations[1][2] == 1
        print("remote mask I/O (bytes, requests, files):", observations)
        lazy = terrio.open_raster(local, lazy=True)
        before_refusal = (Handler.sent, Handler.requests)
        for suffix in ["pmtiles", "mbtiles"]:
            path = tmp_path / f"masked.{suffix}"
            with pytest.raises(ValueError, match="dataset masks"):
                lazy.write(str(path))
            assert not path.exists()
        assert (Handler.sent, Handler.requests) == before_refusal
    finally:
        server.shutdown()
        server.server_close()
        thread.join()


def test_palette_sidecar_destination_and_malformed_tables(tmp_path, gdal):
    from pathlib import Path

    source = terrio.Raster.from_array(np.ones((2, 2), dtype="uint8"), (0, 1, 0, 2, 0, -1))
    palette = tmp_path / "palette.tif"
    source.write(str(palette), colormap=[[0, 0, 0, 255], [10, 20, 30, 255]])
    dataset = gdal.Open(str(palette), gdal.GA_Update)
    with gdal.config_option("GDAL_TIFF_INTERNAL_MASK", "NO"):
        dataset.CreateMaskBand(gdal.GMF_PER_DATASET)
        dataset.GetRasterBand(1).GetMaskBand().WriteArray(
            np.array([[0, 255], [255, 255]], dtype="uint8")
        )
    dataset = None
    mask_path = Path(str(palette) + ".msk")
    original = mask_path.read_bytes()
    with pytest.raises(ValueError, match="input"):
        terrio.attach_palette(source, str(palette), str(mask_path), format="GTiff", overwrite=True)
    assert mask_path.read_bytes() == original
    for invalid in [[[1, 2, 3]], [[0, 0, 0, 256]], np.array([[1, 2, 3]])]:
        with pytest.raises(ValueError, match="RGBA"):
            terrio.read_color_table(invalid)
    lazy = terrio.open_raster(str(palette), lazy=True)
    import io
    import zipfile

    with zipfile.ZipFile(io.BytesIO(lazy.to_bytes("output.png"))) as archive:
        assert archive.read("output.png").startswith(b"\x89PNG")
    lazy.write(str(tmp_path / "output.png"))
