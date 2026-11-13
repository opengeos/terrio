"""GeoJSON names retained CRSs or refuses a write that would silently relabel coordinates."""

import json
import os
import subprocess
import sys
import warnings
from pathlib import Path

import pytest

import terrio

CUSTOM = (
    'PROJCS["custom TM",GEOGCS["WGS 84",DATUM["WGS_1984",'
    'SPHEROID["WGS 84",6378137,298.257223563]],PRIMEM["Greenwich",0],'
    'UNIT["degree",0.0174532925199433]],PROJECTION["Transverse_Mercator"],'
    'PARAMETER["latitude_of_origin",0],PARAMETER["central_meridian",-93.5],'
    'PARAMETER["scale_factor",1],PARAMETER["false_easting",0],'
    'PARAMETER["false_northing",0],UNIT["metre",1]]'
)
PROJ = "+proj=tmerc +datum=WGS84 +lon_0=-93.5 +k=1 +units=m +no_defs"
LOCAL = 'LOCAL_CS["site grid",LOCAL_DATUM["site",32767],UNIT["metre",1]]'
POINTS = {
    "type": "FeatureCollection",
    "features": [
        {
            "type": "Feature",
            "properties": {"id": 1},
            "geometry": {"type": "Point", "coordinates": [12000, 34000]},
        }
    ],
}


@pytest.mark.parametrize("definition", ["EPSG:32615", "ESRI:102003", CUSTOM, PROJ, LOCAL])
@pytest.mark.parametrize("surface", ["layer", "bytes", "convert", "cli", "pyogrio"])
def test_geojson_never_silently_drops_a_known_crs(tmp_path, definition, surface):
    """Name authority CRSs and refuse unnamed definitions on every writing surface.

    Args:
        tmp_path: Directory for the source and output fixtures.
        definition: The source layer's CRS.
        surface: The public export method being exercised.
    """
    layer = terrio.Layer.from_geojson(POINTS, name="points", crs=definition)
    source = tmp_path / "source.gpkg"
    layer.write(str(source))
    output = tmp_path / "points.geojson"
    named = definition.startswith(("EPSG:", "ESRI:"))

    def write():
        """Export the fixture through the selected surface."""
        if surface == "layer":
            layer.write(str(output))
        elif surface == "bytes":
            output.write_bytes(layer.to_bytes(output.name))
        elif surface == "convert":
            terrio.convert(str(source), str(output))
        elif surface == "pyogrio":
            pytest.importorskip("geopandas")
            import terrio.pyogrio as tpg

            tpg.write_dataframe(layer.to_geopandas(), output)
        else:
            result = subprocess.run(
                [sys.executable, "-m", "terrio", "convert", str(source), str(output)],
                capture_output=True,
                text=True,
                timeout=30,
            )
            if named:
                assert result.returncode == 0, result.stderr
            else:
                assert result.returncode != 0
                assert "GeoJSON cannot name" in result.stderr
                assert "RFC7946=YES" in result.stderr

    if named:
        with warnings.catch_warnings():
            warnings.simplefilter("error")
            write()
        document = json.loads(output.read_text())
        authority, code = definition.split(":")
        assert document["crs"]["properties"]["name"] == f"urn:ogc:def:crs:{authority}::{code}"
        assert document["features"][0]["geometry"]["coordinates"] == [12000, 34000]
        assert terrio.open_vector(str(output))[0].crs == definition
        assert terrio.open_stream(str(output)).crs == definition
    else:
        if surface == "cli":
            write()
        else:
            with pytest.raises(terrio.UnsupportedError, match="GeoJSON cannot name.*RFC7946=YES"):
                write()
        assert not output.exists()


@pytest.mark.parametrize("definition", ["EPSG:32615", "ESRI:102003", CUSTOM, PROJ, LOCAL])
def test_rfc7946_reprojects_or_refuses_untransformable_crs(tmp_path, definition):
    """The explicit RFC 7946 option transforms coordinates, never just their label.

    Args:
        tmp_path: Directory for the source and output datasets.
        definition: A named, custom or engineering source CRS.
    """
    pyproj = pytest.importorskip("pyproj")
    source = tmp_path / "source.gpkg"
    terrio.Layer.from_geojson(POINTS, name="points", crs=definition).write(str(source))
    output = tmp_path / "rfc.geojson"
    result = subprocess.run(
        [
            sys.executable,
            "-m",
            "terrio",
            "convert",
            str(source),
            str(output),
            "--lco",
            "RFC7946=YES",
        ],
        capture_output=True,
        text=True,
        timeout=30,
    )
    if definition == LOCAL:
        assert result.returncode != 0
        assert "localcs" in result.stderr.lower()
        return
    assert result.returncode == 0, result.stderr
    document = json.loads(output.read_text())
    assert "crs" not in document
    expected = pyproj.Transformer.from_crs(definition, "EPSG:4326", always_xy=True).transform(
        12000, 34000
    )
    actual = document["features"][0]["geometry"]["coordinates"]
    assert actual == pytest.approx(expected, abs=1e-6)
    assert terrio.open_vector(str(output))[0].crs == "EPSG:4326"


@pytest.mark.parametrize("definition", ["EPSG:32615", "ESRI:102003"])
@pytest.mark.parametrize("surface", ["layer", "bytes", "convert", "pyogrio"])
def test_real_boundary_geojson_crs_matches_gdal(tmp_path, definition, surface):
    """Compare real US boundary exports with GDAL's projected source coordinates.

    Args:
        tmp_path: Directory for the projected fixture and GeoJSON export.
        definition: The projected CRS assigned through GDAL reprojection.
        surface: The public writing surface being compared.
    """
    gdal = pytest.importorskip("osgeo.gdal")
    ogr = pytest.importorskip("osgeo.ogr")
    shapely = pytest.importorskip("shapely")
    np = pytest.importorskip("numpy")
    data = Path(
        os.environ.get("TERRIO_TEST_DATA", Path(__file__).resolve().parents[3] / "terrio-data")
    )
    countries = data / "countries.gpkg"
    if not countries.exists():
        pytest.skip("needs countries.gpkg from scripts/fetch_test_data.sh")
    source = tmp_path / "projected.gpkg"
    output = tmp_path / "boundary.geojson"
    with gdal.ExceptionMgr(), ogr.ExceptionMgr():
        gdal.VectorTranslate(
            str(source),
            str(countries),
            format="GPKG",
            dstSRS=definition,
            where="ADM0_A3 = 'USA'",
            selectFields=["NAME", "ISO_A3", "POP_EST"],
            layerName="boundary",
        )
        reference = gdal.OpenEx(str(source), gdal.OF_VECTOR)
        expected_layer = reference.GetLayer(0)
        assert expected_layer.GetFeatureCount() == 1
        expected = expected_layer.GetNextFeature()
        layer = terrio.open_vector(str(source))[0]
        if surface == "layer":
            layer.write(str(output))
        elif surface == "bytes":
            output.write_bytes(layer.to_bytes(output.name))
        elif surface == "convert":
            terrio.convert(str(source), str(output))
        else:
            pytest.importorskip("geopandas")
            import terrio.pyogrio as tpg

            tpg.write_dataframe(layer.to_geopandas(), output)
        result = gdal.OpenEx(str(output), gdal.OF_VECTOR)
        actual_layer = result.GetLayer(0)
        assert actual_layer.GetFeatureCount() == 1
        srs = actual_layer.GetSpatialRef()
        authority, code = definition.split(":")
        assert srs.GetAuthorityName(None) == authority
        assert srs.GetAuthorityCode(None) == code
        actual = actual_layer.GetNextFeature()
        assert actual.items() == expected.items()
        coords = [
            shapely.get_coordinates(
                shapely.from_wkb(bytes(feature.GetGeometryRef().ExportToIsoWkb()))
            )
            for feature in (actual, expected)
        ]
        assert coords[0].shape == coords[1].shape
        maximum = float(np.max(np.abs(coords[0] - coords[1])))
        assert maximum <= 1e-7
        print(f"{definition} {surface}: maximum coordinate difference {maximum:.9g} m")
