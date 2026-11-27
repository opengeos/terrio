# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT
"""Untrusted container names must not grant access to host files or unmapped sources."""

import json
import threading
import zipfile
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from io import BytesIO

import numpy as np
import pytest

import terrio


def collection(source):
    """Return a one-point GeoJSON fixture with distinguishable provenance.

    Args:
        source: Provenance label stored in the feature.

    Returns:
        str: Serialized GeoJSON feature collection.
    """
    return json.dumps(
        {
            "type": "FeatureCollection",
            "features": [
                {
                    "type": "Feature",
                    "properties": {"source": source},
                    "geometry": {"type": "Point", "coordinates": [1, 2]},
                }
            ],
        }
    )


def test_rooted_archive_member_reads_extracted_data(tmp_path):
    """Use the extracted member, never a host file sharing its stored absolute name.

    Args:
        tmp_path: Temporary directory containing both controlled copies.
    """
    outside = tmp_path / "outside.geojson"
    outside.write_text(collection("host"))
    path = tmp_path / "archive.zip"
    with zipfile.ZipFile(path, "w") as archive:
        archive.writestr(str(outside), collection("archive"))
    layer = terrio.open_vector(str(path)).layers[0]
    actual = layer.to_geojson()
    if isinstance(actual, str):
        actual = json.loads(actual)
    assert actual["features"][0]["properties"]["source"] == "archive"
    assert json.loads(outside.read_text())["features"][0]["properties"]["source"] == "host"


def store(value):
    """Create the files of a minimal Zarr v2 store.

    Args:
        value: Byte value filling the array.

    Returns:
        dict: Relative store file names and their bytes.
    """
    return {
        ".zgroup": json.dumps({"zarr_format": 2}).encode(),
        "b/.zarray": json.dumps(
            {
                "zarr_format": 2,
                "shape": [2, 2],
                "chunks": [2, 2],
                "dtype": "|u1",
                "compressor": None,
                "fill_value": 0,
                "order": "C",
                "filters": None,
            }
        ).encode(),
        "b/.zattrs": json.dumps({"_ARRAY_DIMENSIONS": ["y", "x"]}).encode(),
        "b/0.0": bytes([value] * 4),
    }


def test_rooted_memory_store_names_do_not_open_a_host_store(tmp_path):
    """Reject absolute store keys even when a valid store exists at that host path.

    Args:
        tmp_path: Temporary directory containing the controlled host store.
    """
    host = tmp_path / "host.zarr"
    for name, data in store(7).items():
        path = host / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
    with pytest.raises(Exception, match="relative"):
        terrio.open_multidim({f"{host}/{name}": data for name, data in store(1).items()})
    np.testing.assert_array_equal(terrio.open_multidim(str(host))["b"].read(), np.full((2, 2), 7))


def test_vrt_source_map_refuses_unmapped_host_paths(tmp_path):
    """Opening uploaded XML with an empty map must not read an existing host raster.

    Args:
        tmp_path: Temporary directory containing the controlled host raster.
    """
    path = tmp_path / "host.tif"
    source = terrio.Raster.from_array(np.full((2, 2), 7, dtype="uint8"), (0, 1, 0, 2, 0, -1))
    source.write(str(path))
    xml = (
        '<VRTDataset rasterXSize="2" rasterYSize="2"><VRTRasterBand dataType="Byte" band="1">'
        f'<SimpleSource><SourceFilename relativeToVRT="0">{path}</SourceFilename>'
        "<SourceBand>1</SourceBand></SimpleSource></VRTRasterBand></VRTDataset>"
    )
    with pytest.raises(Exception, match="supplied|mapped"):
        terrio.open_vrt(xml, {}).read(1)
    with pytest.raises(Exception, match="supplied|mapped"):
        terrio.open_vrt(xml, {"other.tif": path.read_bytes()})
    mapped = terrio.open_vrt(xml, {str(path): path.read_bytes()})
    np.testing.assert_array_equal(mapped.read(1), np.full((2, 2), 7))
    vrt = tmp_path / "trusted.vrt"
    vrt.write_text(xml)
    np.testing.assert_array_equal(terrio.open_raster(str(vrt)).read(1), np.full((2, 2), 7))


@pytest.mark.parametrize("lazy", [False, True])
def test_stac_vrt_asset_names_keep_selector_dispatch(tmp_path, lazy):
    """Resolve STAC asset keys before interpreting their VRT filename suffix.

    Args:
        tmp_path: Temporary directory holding the item and its raster asset.
        lazy: Whether to defer raster reads.
    """
    values = np.full((2, 2), 7, dtype="uint8")
    terrio.Raster.from_array(values, (0, 1, 0, 2, 0, -1)).write(str(tmp_path / "source.tif"))
    (tmp_path / "source.vrt").write_text(
        '<VRTDataset rasterXSize="2" rasterYSize="2"><VRTRasterBand dataType="Byte">'
        '<SimpleSource><SourceFilename relativeToVRT="1">source.tif</SourceFilename>'
        "<SourceBand>1</SourceBand></SimpleSource></VRTRasterBand></VRTDataset>"
    )
    item = tmp_path / "item.json"
    item.write_text(
        json.dumps(
            {
                "type": "Feature",
                "stac_version": "1.0.0",
                "assets": {"dem.vrt": {"href": "source.vrt"}},
            }
        )
    )
    for selector in (f'STAC:"{item}":dem.vrt', f"{item}!dem.vrt"):
        np.testing.assert_array_equal(terrio.open_raster(selector, lazy=lazy).read(1), values)


@pytest.mark.parametrize("lazy", [False, True])
@pytest.mark.parametrize("suffix", ["", "?token=synthetic", "#fragment"])
@pytest.mark.parametrize("directory", ["", "collections/", "search/"])
def test_direct_remote_vrt_provenance_and_redirect_base(tmp_path, lazy, suffix, directory):
    """Keep deferred remote sources confined and resolve redirected relative sources.

    Args:
        tmp_path: Temporary directory containing the controlled local source.
        lazy: Whether to defer raster reads.
        suffix: Query or fragment attached to the caller-selected VRT location.
        directory: Optional STAC-looking parent path for the VRT location.
    """
    from terrio import rio

    path = tmp_path / "host.tif"
    values = np.full((2, 2), 7, dtype="uint8")
    terrio.Raster.from_array(values, (0, 1, 0, 2, 0, -1)).write(str(path))
    pixels = path.read_bytes()
    requests = []

    class Handler(BaseHTTPRequestHandler):
        """Serve a redirected public VRT and a document naming a local file."""

        def log_message(self, *_):
            """Suppress test server logs.

            Args:
                _: Ignored HTTP log format and arguments.
            """

        def do_GET(self):
            """Serve controlled document and raster responses."""
            requests.append(self.path)
            self.path = self.path.split("?", 1)[0]
            if directory:
                self.path = self.path.replace(f"/{directory}", "/", 1)
            if self.path in ("/entry.vrt", "/chain.vrt", "/archive-chain.zip"):
                self.send_response(302)
                target = "/nested/remote.vrt"
                if self.path != "/entry.vrt":
                    redirect_name = "archive-back" if self.path == "/archive-chain.zip" else "back"
                    target = f"{bounce}/{redirect_name}"
                self.send_header("Location", target)
                self.send_header("Content-Length", "0")
                self.end_headers()
                return
            if self.path == "/nested/source.tif":
                body = pixels
            elif self.path == "/returned.zip":
                body = archive
            elif self.path in ("/blocked.vrt", "/nested/remote.vrt", "/returned.vrt"):
                blocked = self.path == "/blocked.vrt"
                source = (
                    str(path)
                    if blocked
                    else "nested/source.tif"
                    if self.path == "/returned.vrt"
                    else "source.tif"
                )
                body = (
                    '<VRTDataset rasterXSize="2" rasterYSize="2">'
                    '<VRTRasterBand dataType="Byte"><SimpleSource>'
                    f'<SourceFilename relativeToVRT="{0 if blocked else 1}">'
                    f"{source}</SourceFilename><SourceBand>1</SourceBand>"
                    "</SimpleSource></VRTRasterBand></VRTDataset>"
                ).encode()
            else:
                self.send_response(404)
                self.send_header("Content-Length", "0")
                self.end_headers()
                return
            self.send_response(200)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            if self.command != "HEAD":
                self.wfile.write(body)

        do_HEAD = do_GET

    class BounceHandler(BaseHTTPRequestHandler):
        """Redirect back to the caller-selected origin."""

        log_message = Handler.log_message

        def do_GET(self):
            """Return a cross-origin redirect to the root server."""
            self.send_response(302)
            target = "returned.zip" if self.path == "/archive-back" else "returned.vrt"
            self.send_header("Location", f"{base}/{target}")
            self.send_header("Content-Length", "0")
            self.end_headers()

    with (
        ThreadingHTTPServer(("127.0.0.1", 0), Handler) as server,
        rio.Env(GDAL_HTTP_HEADERS="", TERRIO_STAC_ALLOW_CREDENTIALED_ASSETS="NO"),
    ):
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            base = f"http://127.0.0.1:{server.server_port}"
            with pytest.raises(Exception, match="local files"):
                terrio.open_raster(f"{base}/{directory}blocked.vrt{suffix}", lazy=lazy).read(1)
            with pytest.raises(Exception, match="local files"):
                terrio.open(f"{base}/{directory}blocked.vrt{suffix}")
            np.testing.assert_array_equal(terrio.open_raster(str(path)).read(1), values)
            opened = terrio.open_raster(f"{base}/{directory}entry.vrt{suffix}", lazy=lazy)
            if lazy:
                assert "/nested/source.tif" not in requests
            np.testing.assert_array_equal(opened.read(1), values)
            assert "/nested/source.tif" in requests
            assert "/source.tif" not in requests
            with ThreadingHTTPServer(("127.0.0.1", 0), BounceHandler) as other:
                bounce = f"http://127.0.0.1:{other.server_port}"
                other_thread = threading.Thread(target=other.serve_forever, daemon=True)
                other_thread.start()
                try:
                    with rio.Env(GDAL_HTTP_HEADERS="Authorization: synthetic-test"):
                        before = requests.count("/nested/source.tif")
                        with pytest.raises(
                            Exception, match="TERRIO_STAC_ALLOW_CREDENTIALED_ASSETS"
                        ):
                            terrio.open_raster(f"{base}/chain.vrt", lazy=lazy).read(1)
                        assert requests.count("/nested/source.tif") == before
                        buffer = BytesIO()
                        with zipfile.ZipFile(buffer, "w", zipfile.ZIP_DEFLATED) as zipped:
                            zipped.writestr(
                                "document.vrt",
                                '<VRTDataset rasterXSize="2" rasterYSize="2">'
                                '<VRTRasterBand dataType="Byte"><SimpleSource>'
                                f'<SourceFilename relativeToVRT="0">{base}/nested/source.tif'
                                "</SourceFilename><SourceBand>1</SourceBand>"
                                "</SimpleSource></VRTRasterBand></VRTDataset>",
                            )
                        archive = buffer.getvalue()
                        member = f"/vsizip/{{{base}/archive-chain.zip}}/document.vrt"
                        assert terrio.vsi_read(member).startswith(b"<VRTDataset")
                        with pytest.raises(
                            Exception, match="TERRIO_STAC_ALLOW_CREDENTIALED_ASSETS"
                        ):
                            terrio.open_raster(member, lazy=lazy).read(1)
                        assert requests.count("/nested/source.tif") == before
                finally:
                    other.shutdown()
                    other_thread.join()
        finally:
            server.shutdown()
            thread.join()


def test_vrt_supplied_as_bytes_resolves_only_within_the_map(tmp_path):
    """A mapped source that is itself a VRT may name only other mapped sources."""
    host = tmp_path / "host.tif"
    terrio.Raster.from_array(np.full((1, 2, 2), 7, "uint8"), (0.0, 1.0, 0.0, 2.0, 0.0, -1.0)).write(
        str(host)
    )
    upload = terrio.Raster.from_array(
        np.full((1, 2, 2), 1, "uint8"), (0.0, 1.0, 0.0, 2.0, 0.0, -1.0)
    ).to_bytes()

    def vrt(source):
        return (
            '<VRTDataset rasterXSize="2" rasterYSize="2"><VRTRasterBand dataType="Byte" band="1">'
            f'<SimpleSource><SourceFilename relativeToVRT="0">{source}</SourceFilename>'
            "<SourceBand>1</SourceBand></SimpleSource></VRTRasterBand></VRTDataset>"
        )

    with pytest.raises(Exception, match="supplied"):
        terrio.open_vrt(vrt("inner.vrt"), {"inner.vrt": vrt(str(host)).encode()})
    nested = {"inner.vrt": vrt("upload.tif").encode(), "upload.tif": upload}
    np.testing.assert_array_equal(
        terrio.open_vrt(vrt("inner.vrt"), nested).read(1), np.full((2, 2), 1)
    )
    with pytest.raises(Exception, match="references itself"):
        terrio.open_vrt(vrt("a.vrt"), {"a.vrt": vrt("a.vrt").encode()})


@pytest.mark.parametrize("prefix", ["/", "\\", "C:/", "C:\\", "./C:/", "../", "x/../../"])
def test_memory_store_names_are_portable_relative_keys(prefix):
    """Reject roots and traversal consistently on every host platform.

    Args:
        prefix: Unsafe prefix to prepend to every store key.
    """
    with pytest.raises(Exception, match="relative"):
        terrio.open_multidim({prefix + name: value for name, value in store(1).items()})


def test_memory_store_normalization_cannot_alias_keys():
    """Reject duplicate normalized keys and preserve a valid nested store."""
    files = {f"nested/{name}": value for name, value in store(3).items()}
    np.testing.assert_array_equal(terrio.open_multidim(files)["b"].read(), np.full((2, 2), 3))
    files["nested/./b/0.0"] = bytes([9] * 4)
    with pytest.raises(Exception, match="unique"):
        terrio.open_multidim(files)


def test_vrt_rpc_dem_uses_supplied_bytes_instead_of_a_host_file(tmp_path):
    """Keep the RPC transformer's independent DEM reader inside the source map.

    Args:
        tmp_path: Directory holding controlled source, valid DEM, and invalid decoy.
    """
    source = terrio.Raster.from_array(
        np.array([[1, 2], [3, 4]], dtype="uint8"), (0, 1, 0, 2, 0, -1), crs="EPSG:4326"
    )
    dem = terrio.Raster.from_array(
        np.zeros((20, 20), dtype="float32"), (-10, 1, 0, 10, 0, -1), crs="EPSG:4326"
    )
    source_path, dem_path, decoy = (
        tmp_path / name for name in ["source.tif", "dem.tif", "private-dem.tif"]
    )
    source.write(str(source_path))
    dem.write(str(dem_path))
    decoy.write_text("CONTROLLED_HOST_DEM_CONTENT")
    metadata = {
        "LINE_OFF": "0",
        "SAMP_OFF": "0",
        "LAT_OFF": "0",
        "LONG_OFF": "0",
        "HEIGHT_OFF": "0",
        "LINE_SCALE": "1",
        "SAMP_SCALE": "1",
        "LAT_SCALE": "1",
        "LONG_SCALE": "1",
        "HEIGHT_SCALE": "1",
    }
    for key, index in [
        ("LINE_NUM_COEFF", 2),
        ("SAMP_NUM_COEFF", 1),
        ("LINE_DEN_COEFF", 0),
        ("SAMP_DEN_COEFF", 0),
    ]:
        metadata[key] = " ".join("1" if i == index else "0" for i in range(20))
    items = "".join(f'<MDI key="{key}">{value}</MDI>' for key, value in metadata.items())
    xml = (
        '<VRTDataset rasterXSize="2" rasterYSize="2" subClass="VRTWarpedDataset">'
        "<SRS>EPSG:4326</SRS><GeoTransform>0,1,0,2,0,-1</GeoTransform>"
        '<VRTRasterBand dataType="Byte"/><GDALWarpOptions>'
        f"<SourceDataset>{source_path}</SourceDataset><Transformer><RPCTransformer>"
        f"<DEMPath>{decoy}</DEMPath><DEMInterpolation>near</DEMInterpolation>"
        f"<Metadata>{items}</Metadata></RPCTransformer></Transformer>"
        '<BandList><BandMapping src="1" dst="1"/></BandList></GDALWarpOptions></VRTDataset>'
    )
    sources = {str(source_path): source_path.read_bytes()}
    with pytest.raises(Exception, match="supplied"):
        terrio.open_vrt(xml, sources)
    sources[str(decoy)] = dem_path.read_bytes()
    actual = terrio.open_vrt(xml, sources).read(1)
    trusted = tmp_path / "trusted-rpc.vrt"
    trusted.write_text(xml.replace(str(decoy), str(dem_path)))
    np.testing.assert_array_equal(actual, terrio.open_raster(str(trusted)).read(1))
    assert np.count_nonzero(actual) > 0
    assert decoy.read_text() == "CONTROLLED_HOST_DEM_CONTENT"


def test_vrt_missing_remote_references_issue_no_requests():
    """Reject remote references even when pixels or overview discovery could open them."""
    requests = []

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_):
            """Suppress test server logs."""

        def do_GET(self):
            """Record attempts to fetch a source that was not supplied."""
            requests.append(self.path)
            self.send_response(404)
            self.send_header("Content-Length", "0")
            self.end_headers()

        do_HEAD = do_GET

    with ThreadingHTTPServer(("127.0.0.1", 0), Handler) as server:
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            url = f"http://127.0.0.1:{server.server_port}/unmapped.tif"
            plain = (
                '<VRTDataset rasterXSize="2" rasterYSize="2">'
                '<VRTRasterBand dataType="Byte"><SimpleSource>'
                f"<SourceFilename>{url}</SourceFilename><SourceBand>1</SourceBand>"
                "</SimpleSource></VRTRasterBand></VRTDataset>"
            )
            warped = (
                '<VRTDataset rasterXSize="2" rasterYSize="2" subClass="VRTWarpedDataset">'
                '<GeoTransform>0,1,0,2,0,-1</GeoTransform><VRTRasterBand dataType="Byte"/>'
                f"<GDALWarpOptions><SourceDataset>{url}</SourceDataset><BandList>"
                '<BandMapping src="1" dst="1"/></BandList></GDALWarpOptions></VRTDataset>'
            )
            pansharpened = (
                '<VRTDataset subClass="VRTPansharpenedDataset"><PansharpeningOptions>'
                f"<PanchroBand><SourceFilename>{url}</SourceFilename></PanchroBand>"
                '<SpectralBand dstBand="1"><SourceFilename>spectral.tif</SourceFilename>'
                "</SpectralBand></PansharpeningOptions></VRTDataset>"
            )
            for xml in [plain, warped, pansharpened]:
                with pytest.raises(Exception, match="supplied|mapped"):
                    terrio.open_vrt(xml, {})
            assert requests == []
        finally:
            server.shutdown()
            thread.join()
