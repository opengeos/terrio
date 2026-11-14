"""Unresolvable authority names cannot silently lose or mislabel vector CRSs."""

import subprocess
import sys
import warnings

import pytest
from test_geojson_write_crs import POINTS

import terrio

ENGINEERING = (
    'LOCAL_CS["Astra Minas Grid",LOCAL_DATUM["Astra Minas",32767],UNIT["metre",1],'
    'AXIS["X",NORTH],AXIS["Y",WEST],AUTHORITY["EPSG","5800"]]'
)


@pytest.mark.parametrize("extension", ["geojson", "topojson"])
@pytest.mark.parametrize("surface", ["path", "bytes", "convert", "cli"])
def test_json_refuses_unresolvable_authorities(tmp_path, extension, surface):
    """Refuse name-only writes before creating output or changing coordinates.

    Args:
        tmp_path: Directory for the source and destination.
        extension: A JSON vector format sharing CRS naming.
        surface: Public writer entry point.
    """
    layer = terrio.Layer.from_geojson(POINTS, name="points", crs=ENGINEERING)
    assert layer.crs == "EPSG:5800"
    source = tmp_path / "points.gpkg"
    layer.write(str(source))
    output = tmp_path / f"points.{extension}"
    if surface == "cli":
        result = subprocess.run(
            [sys.executable, "-m", "terrio", "convert", str(source), str(output)],
            capture_output=True,
            text=True,
            timeout=30,
        )
        assert result.returncode != 0
        assert "cannot name this layer's CRS" in result.stderr
    else:
        with pytest.raises(terrio.UnsupportedError, match="cannot name this layer's CRS"):
            if surface == "path":
                layer.write(str(output))
            elif surface == "bytes":
                layer.to_bytes(output.name)
            else:
                terrio.convert(str(source), str(output))
    assert not output.exists()


def test_unknown_gml_code_warns_on_write_and_read(tmp_path):
    """Retain the legacy GML name and XY coordinates while diagnosing lost CRS metadata.

    Args:
        tmp_path: Directory for the GML file.
    """
    layer = terrio.Layer.from_geojson(POINTS, name="points", crs=ENGINEERING)
    path = tmp_path / "points.gml"
    with pytest.warns(UserWarning, match="writes unresolved srsName 'EPSG:5800'"):
        layer.write(str(path))
    assert 'srsName="EPSG:5800"' in path.read_text()
    with pytest.warns(UserWarning, match="srsName 'EPSG:5800' cannot be resolved"):
        restored = terrio.open_vector(str(path))[0]
    assert restored.crs is None
    feature = restored.to_geojson()["features"][0]
    assert feature["geometry"] == POINTS["features"][0]["geometry"]
    assert feature["properties"]["id"] == 1


@pytest.mark.parametrize("name", ["urn:ogc:def:crs:EPSG::5800", "urn:ogc:def:crs:FOO::1"])
@pytest.mark.parametrize("surface", ["path", "bytes", "stream"])
def test_unresolved_gml_names_warn_without_dropping_features(tmp_path, name, surface):
    """Warn about unresolved declarations while retaining the original features.

    Args:
        tmp_path: Directory for the input file.
        name: An authority identifier unavailable to the reader.
        surface: Vector input entry point.
    """
    source = terrio.Layer.from_geojson(POINTS, name="points", crs="EPSG:32615")
    data = source.to_bytes("points.gml").replace(b"urn:ogc:def:crs:EPSG::32615", name.encode())
    path = tmp_path / "points.gml"
    path.write_bytes(data)
    with pytest.warns(UserWarning, match="cannot be resolved") as caught:
        if surface == "path":
            layer = terrio.open_vector(str(path))[0]
        elif surface == "bytes":
            layer = terrio.open_bytes(data, path.name)[0]
        else:
            layer = terrio.open_stream(str(path)).read_all()
    assert len(caught) == 1
    assert name in str(caught[0].message)
    assert layer.crs is None
    feature = layer.to_geojson()["features"][0]
    assert feature["geometry"] == POINTS["features"][0]["geometry"]
    assert feature["properties"]["id"] == 1


def test_pyogrio_refuses_unresolvable_authorities(tmp_path):
    """Apply the same naming policy to dataframe writes.

    Args:
        tmp_path: Directory for the refused destination.
    """
    gpd = pytest.importorskip("geopandas")
    from terrio import pyogrio

    frame = gpd.GeoDataFrame.from_features(POINTS, crs=ENGINEERING)
    path = tmp_path / "points.geojson"
    with pytest.raises(terrio.UnsupportedError, match="cannot name this layer's CRS"):
        pyogrio.write_dataframe(frame, path)
    assert not path.exists()


def test_gml_read_warning_redacts_credentials_and_known_crs_is_quiet(tmp_path):
    """Keep diagnostics private and valid GML reads free of new warnings.

    Args:
        tmp_path: Directory for the GML fixture.
    """
    layer = terrio.Layer.from_geojson(POINTS, name="points", crs="EPSG:32615")
    data = layer.to_bytes("points.gml")
    with warnings.catch_warnings(record=True) as caught:
        warnings.simplefilter("always")
        assert terrio.open_bytes(data, "points.gml")[0].crs == "EPSG:32615"
    assert not caught
    data = data.replace(
        b"urn:ogc:def:crs:EPSG::32615", b"https://example.org/crs?token=PRIVATE-GML-CRS"
    )
    with pytest.warns(UserWarning, match="cannot be resolved") as caught:
        assert terrio.open_bytes(data, "points.gml")[0].crs is None
    assert "PRIVATE-GML-CRS" not in str(caught[0].message)
    assert "<redacted>" in str(caught[0].message)
