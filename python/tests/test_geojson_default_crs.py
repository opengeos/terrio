"""GeoJSON without a `crs` member: the CRS GDAL gives it, through terrio's own APIs.

GDAL's GeoJSON driver uses EPSG:4979 when the layer's geometry type has heights, EPSG:4326
otherwise ("If there is none defined, we use 4326 / 4979"); GeoJSONSeq stays WGS 84. The values
were measured with pyogrio 0.13.0 (GDAL 3.12.4) `read_info` on the same documents (a 26-file
GeoJSON matrix compared with GDAL); `test_pyogrio_parity.py` compares with pyogrio itself when
it is installed. Both readers are checked: whole documents (`open_vector`) and streams
(`open_stream`).
"""

import json

import pytest

import terrio

P2 = {"type": "Point", "coordinates": [10.0, 20.0]}
P3 = {"type": "Point", "coordinates": [10.0, 20.0, 30.0]}
LINE3 = {"type": "LineString", "coordinates": [[10, 20, 1], [11, 21, 2]]}
CASES = {
    "the USGS feed's shape: a 3-D point": ([P3], "EPSG:4979"),
    "a 2-D point": ([P2], "EPSG:4326"),
    "2-D, then 3-D": ([P2, P3], "EPSG:4979"),
    "mixed types with heights": ([P3, LINE3], "EPSG:4326"),
    "a 2-D first position": (
        [{"type": "LineString", "coordinates": [[1, 2], [3, 4, 5]]}],
        "EPSG:4326",
    ),
    "no geometry": ([None], "EPSG:4326"),
}


def _write(path, geometries, **members):
    features = [
        {"type": "Feature", "properties": {"i": i}, "geometry": g} for i, g in enumerate(geometries)
    ]
    path.write_text(json.dumps({"type": "FeatureCollection", **members, "features": features}))
    return str(path)


@pytest.mark.parametrize("name", list(CASES))
def test_whole_and_streamed_reads_take_gdals_default_crs(tmp_path, name):
    geometries, expected = CASES[name]
    path = _write(tmp_path / "f.geojson", geometries)
    assert terrio.open_vector(path).layers[0].crs == expected
    assert terrio.open_stream(path).crs == expected


def test_a_crs_member_and_a_sequence_keep_theirs(tmp_path):
    named = {"crs": {"type": "name", "properties": {"name": "urn:ogc:def:crs:EPSG::32633"}}}
    path = _write(tmp_path / "utm.geojson", [P3], **named)
    assert terrio.open_vector(path).layers[0].crs == "EPSG:32633"
    assert terrio.open_stream(path).crs == "EPSG:32633"
    seq = tmp_path / "f.geojsonl"
    seq.write_text(json.dumps({"type": "Feature", "properties": {}, "geometry": P3}) + "\n")
    assert terrio.open_vector(str(seq)).layers[0].crs == "EPSG:4326"
    assert terrio.open_stream(str(seq)).crs == "EPSG:4326"
