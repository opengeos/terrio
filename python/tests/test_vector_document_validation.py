"""Wrong document kinds must not become empty vector datasets or empty features."""

import io
import json
import struct
import zipfile

import pytest

import terrio

HTML = b"<html><head><title>404 Not Found</title></head><body>Missing</body></html>"
SVG = b'<svg xmlns="http://www.w3.org/2000/svg"><path d="M0 0"/></svg>'
EMPTY = {
    "kml": b'<kml xmlns="http://www.opengis.net/kml/2.2"><Document/></kml>',
    "gpx": b'<gpx version="1.1" creator="test"/>',
    "osm": b'<osm version="0.6"/>',
    "geojsonl": b"",
}
POINT = {"type": "Point", "coordinates": [10, 46]}
FEATURE = {"type": "Feature", "properties": {"n": 1}, "geometry": POINT}
COLLECTION = {"type": "FeatureCollection", "features": [FEATURE]}
BAD = [
    (extension, label, data)
    for extension in ("kml", "gpx", "osm")
    for label, data in [
        ("html", HTML),
        ("svg", SVG),
        ("empty", b""),
        ("feature", json.dumps(FEATURE).encode()),
        ("csv", b"x,y\n10,46\n"),
        *[(kind, doc) for kind, doc in EMPTY.items() if kind != extension and doc],
    ]
] + [
    ("geojsonl", label, data)
    for label, data in [
        ("html", HTML),
        ("svg", SVG),
        *[(kind, doc) for kind, doc in EMPTY.items() if kind != "geojsonl"],
        ("collection", json.dumps(COLLECTION).encode()),
        ("arbitrary", b'{"hello":"world"}'),
        ("null", b"null"),
        ("array", b"[]"),
        ("scalar", b"123"),
    ]
]


def read_document(path, data, surface):
    """Read a document through the requested public entry point.

    Args:
        path: Dataset filename in a temporary directory.
        data: Bytes to write and read.
        surface: Path, generic opener, byte buffer, or stream.

    Returns:
        list: Layers returned by the reader.
    """
    path.write_bytes(data)
    if surface == "bytes":
        return list(terrio.open_bytes(data, path.name))
    if surface == "generic":
        return list(terrio.open(str(path)))
    if surface == "stream":
        return [terrio.open_stream(str(path)).read_all()]
    return list(terrio.open_vector(str(path)))


@pytest.mark.parametrize("surface", ["path", "bytes", "generic", "stream"])
@pytest.mark.parametrize("extension,label,data", BAD)
def test_wrong_document_is_refused(tmp_path, extension, label, data, surface):
    """Reject HTML, another format, or a non-feature sequence record.

    Args:
        tmp_path: Temporary dataset directory.
        extension: Format selected by the filename.
        label: Description of the invalid payload.
        data: Payload that is not a document of the selected format.
        surface: Public read entry point.
    """
    path = tmp_path / f"{label}.{extension}"
    # The existing PBF header-size budget also rejects non-XML text immediately.
    error = (
        terrio.ResourceLimitError
        if extension == "osm" and label in ("feature", "csv")
        else terrio.InvalidDataError
    )
    with pytest.raises(error):
        read_document(path, data, surface)


@pytest.mark.parametrize("surface", ["path", "bytes", "generic"])
@pytest.mark.parametrize("extension,layers", [("kml", 0), ("gpx", 5), ("osm", 5), ("geojsonl", 1)])
def test_valid_empty_document_keeps_its_layers(tmp_path, extension, layers, surface):
    """Distinguish valid empty documents from missing or incorrect document roots.

    Args:
        tmp_path: Temporary dataset directory.
        extension: Selected vector format.
        layers: Existing layer count for an empty document.
        surface: Public read entry point.
    """
    result = read_document(tmp_path / f"empty.{extension}", EMPTY[extension], surface)
    assert len(result) == layers
    assert all(len(layer) == 0 for layer in result)


@pytest.mark.parametrize("extension,layers", [("kml", 0), ("gpx", 5), ("osm", 5)])
def test_prefixed_xml_root_is_accepted(tmp_path, extension, layers):
    """Continue accepting a namespace-qualified format root.

    Args:
        tmp_path: Temporary dataset directory.
        extension: XML vector format.
        layers: Existing count for its empty document.
    """
    body = "<Document/>" if extension == "kml" else ""
    data = f'<x:{extension} xmlns:x="urn:example">{body}</x:{extension}>'.encode()
    result = read_document(tmp_path / f"prefixed.{extension}", data, "bytes")
    assert len(result) == layers


@pytest.mark.parametrize("surface", ["path", "bytes", "stream"])
@pytest.mark.parametrize("stac", [False, True])
@pytest.mark.parametrize("bad", [COLLECTION, {"hello": "world"}, None, []])
def test_later_sequence_record_is_validated(tmp_path, surface, stac, bad):
    """Validate later records in both the lightweight probe and STAC scan paths.

    Args:
        tmp_path: Temporary dataset directory.
        surface: Public read entry point.
        stac: Whether the first record selects STAC schema inference.
        bad: Invalid second sequence record.
    """
    first = dict(FEATURE)
    if stac:
        first["stac_version"] = "1.0.0"
        first["id"] = "one"
    data = (json.dumps(first) + "\n" + json.dumps(bad) + "\n").encode()
    with pytest.raises(terrio.InvalidDataError, match="GeoJSONSeq"):
        read_document(tmp_path / "later.geojsonl", data, surface)


@pytest.mark.parametrize("surface", ["path", "bytes", "stream"])
@pytest.mark.parametrize("valid_last", [False, True])
def test_sequence_duplicate_type_uses_last_member(tmp_path, surface, valid_last):
    """Keep JSON's last duplicate member rule when probing sequence types.

    Args:
        tmp_path: Temporary dataset directory.
        surface: Public read entry point.
        valid_last: Whether the final type member denotes a valid feature.
    """
    types = '"type":"other","type":"Feature"' if valid_last else '"type":"Feature","type":"other"'
    record = "{" + types + ',"properties":{"n":2},"geometry":null}'
    data = (json.dumps(FEATURE) + "\n" + record + "\n").encode()
    path = tmp_path / "duplicate.geojsonl"
    if valid_last:
        layers = read_document(path, data, surface)
        assert len(layers[0]) == 2
        assert layers[0].to_geojson()["features"][1]["properties"]["n"] == 2
    else:
        with pytest.raises(terrio.InvalidDataError, match="GeoJSONSeq"):
            read_document(path, data, surface)


@pytest.mark.parametrize("surface", ["path", "bytes", "stream"])
def test_sequence_geometry_and_null_feature_remain_valid(tmp_path, surface):
    """Read bare geometry and null-geometry features without losing properties.

    Args:
        tmp_path: Temporary dataset directory.
        surface: Public read entry point.
    """
    records = [POINT, {"type": "Feature", "properties": {"n": 2}, "geometry": None}]
    data = b"".join(b"\x1e" + json.dumps(record).encode() + b"\n" for record in records)
    layer = read_document(tmp_path / "valid.geojsonl", data, surface)[0]
    features = layer.to_geojson()["features"]
    assert features[0]["geometry"] == POINT
    assert features[1]["geometry"] is None
    assert features[1]["properties"]["n"] == 2


@pytest.mark.parametrize("linked", [False, True])
def test_kmz_layer_documents_require_kml_roots(tmp_path, linked):
    """Check main documents and linked layer documents inside KMZ archives.

    Args:
        tmp_path: Temporary dataset directory.
        linked: Whether the wrong document is the target of a NetworkLink.
    """
    archive = io.BytesIO()
    with zipfile.ZipFile(archive, "w") as output:
        if linked:
            output.writestr(
                "doc.kml",
                "<kml><Document><NetworkLink><Link><href>files/other.kml</href></Link></NetworkLink></Document></kml>",
            )
            output.writestr("files/other.kml", EMPTY["gpx"])
        else:
            output.writestr("doc.kml", HTML)
    data = archive.getvalue()
    for surface in ("path", "bytes"):
        with pytest.raises(terrio.InvalidDataError, match="KML.*root element"):
            read_document(tmp_path / "wrong.kmz", data, surface)


@pytest.mark.parametrize("extension", ["osm", "osm.pbf"])
def test_zero_byte_osm_is_not_an_empty_dataset(tmp_path, extension):
    """Require a real XML root or PBF header even when a buffer has zero bytes.

    Args:
        tmp_path: Temporary dataset directory.
        extension: OSM XML or PBF filename.
    """
    with pytest.raises(terrio.InvalidDataError, match="OSM.*empty input"):
        read_document(tmp_path / f"empty.{extension}", b"", "bytes")


@pytest.mark.parametrize("surface", ["path", "bytes", "generic"])
def test_pbf_data_without_header_is_refused(tmp_path, surface):
    """Refuse framed OSM data when the required document header is absent.

    Args:
        tmp_path: Temporary dataset directory.
        surface: Public open entry point.
    """
    blob = b"\x0a\x00"
    header = b"\x0a\x07OSMData\x18" + bytes([len(blob)])
    data = struct.pack(">I", len(header)) + header + blob
    with pytest.raises(terrio.InvalidDataError, match="OSM PBF.*missing OSMHeader"):
        read_document(tmp_path / "headerless.osm.pbf", data, surface)


def test_empty_pbf_header_keeps_five_layers(tmp_path):
    """Accept an OSMHeader blob with no data blobs as a genuine empty PBF.

    Args:
        tmp_path: Temporary dataset directory.
    """
    required = b"OsmSchema-V0.6"
    block = b"\x22" + bytes([len(required)]) + required
    blob = b"\x0a" + bytes([len(block)]) + block
    header = b"\x0a\x09OSMHeader\x18" + bytes([len(blob)])
    data = struct.pack(">I", len(header)) + header + blob
    for surface in ("path", "bytes", "generic"):
        layers = read_document(tmp_path / "empty.osm.pbf", data, surface)
        assert len(layers) == 5
        assert all(len(layer) == 0 for layer in layers)


def test_empty_sequence_stream_is_valid(tmp_path):
    """Keep a sequence of zero records readable through the streaming API.

    Args:
        tmp_path: Temporary dataset directory.
    """
    layers = read_document(tmp_path / "empty.geojsonl", b"", "stream")
    assert len(layers[0]) == 0
