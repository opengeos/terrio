# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT
"""Untrusted container names must not grant access to host files or unmapped sources."""

import json
import threading
import zipfile
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

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
