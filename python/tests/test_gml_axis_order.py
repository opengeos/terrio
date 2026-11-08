"""GML authority axis order follows the CRS, including projected northing-first axes."""

import pytest

import terrio


@pytest.mark.parametrize(
    "code,authority,expected",
    [
        (6708, (4823832.72, 561996.39), (561996.39, 4823832.72)),
        (7792, (561996.39, 4823832.72), (561996.39, 4823832.72)),
        (4326, (43.56, 12.24), (12.24, 43.56)),
    ],
)
@pytest.mark.parametrize(
    "template,uses_authority",
    [
        ("urn:ogc:def:crs:EPSG::{code}", True),
        ("urn:x-ogc:def:crs:EPSG:{code}", True),
        ("http://www.opengis.net/def/crs/EPSG/0/{code}", True),
        ("http://www.opengis.net/gml/srs/epsg.xml#{code}", False),
        ("EPSG:{code}", False),
    ],
)
def test_gml_axis_order_across_paths_bytes_and_writes(
    tmp_path, code, authority, expected, template, uses_authority
):
    """Preserve GIS XY and height across native GML entrypoints (GDAL 3.12.1 reference).

    Args:
        tmp_path: Temporary directory supplied by pytest.
        code: EPSG CRS identifier.
        authority: Coordinates in the CRS's declared axis order.
        expected: Expected traditional GIS XY coordinates.
        template: GML srsName identifier template.
        uses_authority: Whether the identifier declares authority axis order.
    """
    position = authority if uses_authority else expected
    text = (
        '<gml:FeatureCollection xmlns:gml="http://www.opengis.net/gml/3.2" xmlns:ex="urn:test">'
        '<gml:featureMember><ex:p gml:id="p.0"><ex:geom>'
        f'<gml:Point srsName="{template.format(code=code)}" srsDimension="3">'
        f"<gml:pos>{position[0]} {position[1]} 7</gml:pos>"
        "</gml:Point></ex:geom></ex:p></gml:featureMember></gml:FeatureCollection>"
    )
    path = tmp_path / "input.gml"
    path.write_text(text)
    for source in [
        terrio.open_vector(str(path)),
        terrio.open_bytes(text.encode(), filename="input.gml"),
    ]:
        layer = source[0]
        assert layer.crs == f"EPSG:{code}"
        assert layer.to_geojson()["features"][0]["geometry"]["coordinates"] == [
            *expected,
            7,
        ]
        # Native GML writes a .gml plus its .xsd sidecar, bundled as ZIP bytes.
        archive = layer.to_bytes("output.gml")
        assert archive.startswith(b"PK")
        restored = terrio.open_bytes(archive, filename="output.zip")[0]
        assert restored.crs == layer.crs
        assert (
            restored.to_geojson()["features"][0]["geometry"]
            == layer.to_geojson()["features"][0]["geometry"]
        )
