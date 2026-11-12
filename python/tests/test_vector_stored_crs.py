"""Stored CRS robustness across vector readers and dataframe/Arrow exports (#784)."""

import sqlite3
import warnings

import pytest

import terrio

POINTS = {
    "type": "FeatureCollection",
    "features": [
        {
            "type": "Feature",
            "properties": {"id": 1},
            "geometry": {"type": "Point", "coordinates": [10, 46]},
        },
    ],
}
CUSTOM = (
    'PROJCS["custom",GEOGCS["WGS 84",DATUM["WGS_1984",'
    'SPHEROID["WGS 84",6378137,298.257223563]],PRIMEM["Greenwich",0],'
    'UNIT["degree",0.0174532925199433]],PROJECTION["Transverse_Mercator"],'
    'PARAMETER["central_meridian",15.5],PARAMETER["scale_factor",0.9996],'
    'PARAMETER["false_easting",500000],UNIT["metre",1]]'
)

# OGC:AnsiDate, exported by PROJ 9.8.1; valid WKT2 that WKT1 cannot express.
TEMPORAL = (
    'TIMECRS["Ansi Date",TDATUM["Epoch time for the ANSI date (1-Jan-1601, 00h00 UTC) '
    'as day 1.",CALENDAR["proleptic Gregorian"],TIMEORIGIN[1600-12-31T00:00:00Z]],'
    'CS[TemporalCount,1],AXIS["time (T)",future,TIMEUNIT["day"]],ID["OGC","AnsiDate"]]'
)


def geopandas_for_surface(surface):
    gpd = pytest.importorskip("geopandas")
    if surface in ("arrow", "stream"):
        pytest.importorskip("pyarrow")
        if not hasattr(gpd.GeoDataFrame, "from_arrow"):
            pytest.skip("GeoDataFrame.from_arrow requires GeoPandas 1.0 or later")
    return gpd


def stored_crs(tmp_path, driver, text, container_definition=CUSTOM):
    path = tmp_path / f"points.{driver}"
    layer = terrio.Layer.from_geojson(POINTS, name="points", crs=container_definition)
    layer.write(str(path))
    if driver == "gpkg":
        with sqlite3.connect(path) as db:
            db.execute(
                "INSERT INTO gpkg_spatial_ref_sys "
                "(srs_name,srs_id,organization,organization_coordsys_id,definition) "
                "VALUES ('test',100000,'NONE',100000,?)",
                (text,),
            )
            db.execute("UPDATE gpkg_geometry_columns SET srs_id=100000")
            db.execute("UPDATE gpkg_contents SET srs_id=100000")
    elif driver == "fgb":
        raw = bytearray(path.read_bytes())
        start = raw.index(b'PROJCS["custom')
        encoded = text.encode()
        assert len(encoded) <= int.from_bytes(raw[start - 4 : start], "little")
        raw[start - 4 : start] = len(encoded).to_bytes(4, "little")
        raw[start : start + len(encoded) + 1] = encoded + b"\0"
        path.write_bytes(raw)
    else:
        path.with_suffix(".prj").write_text(text)
    return path


@pytest.mark.parametrize("driver", ["gpkg", "fgb", "shp"])
def test_non_crs_stored_text_is_warned_and_discarded(tmp_path, driver):
    path = stored_crs(tmp_path, driver, "not a coordinate reference system")
    with pytest.warns(UserWarning, match="no CRS definition.*not a coordinate"):
        layer = terrio.open_vector(str(path))[0]
    assert layer.crs is None
    assert len(layer) == 1


@pytest.mark.parametrize("driver", ["gpkg", "fgb", "shp"])
@pytest.mark.parametrize("surface", ["geopandas", "arrow", "stream"])
def test_unparseable_stored_definition_exports_without_crs(tmp_path, driver, surface):
    gpd = geopandas_for_surface(surface)
    path = stored_crs(tmp_path, driver, 'PROJCS["x"]')
    layer = terrio.open_vector(str(path))[0]
    assert layer.crs == 'PROJCS["x"]'
    with pytest.warns(UserWarning, match="CRS definition cannot be parsed"):
        if surface == "geopandas":
            result = layer.to_geopandas()
        else:
            result = gpd.GeoDataFrame.from_arrow(layer.to_arrow() if surface == "arrow" else layer)
    assert layer.crs == 'PROJCS["x"]'
    assert result.crs is None
    assert list(result["id"]) == [1]
    assert list(result.geometry.x) == [10]


@pytest.mark.parametrize("driver", ["gpkg", "fgb", "shp"])
@pytest.mark.parametrize("definition", [CUSTOM, 'LOCAL_CS["site grid",UNIT["metre",1]]', TEMPORAL])
@pytest.mark.parametrize("surface", ["geopandas", "arrow", "stream", "pyogrio"])
def test_valid_custom_stored_definitions_survive_exports(tmp_path, driver, definition, surface):
    gpd = geopandas_for_surface(surface)
    from pyproj import CRS

    import terrio.pyogrio as tpg

    path = stored_crs(tmp_path, driver, definition)
    layer = terrio.open_vector(str(path))[0]
    assert layer.crs is not None
    if surface == "pyogrio":
        result = tpg.read_dataframe(path)
    elif surface == "geopandas":
        result = layer.to_geopandas()
    else:
        result = gpd.GeoDataFrame.from_arrow(layer.to_arrow() if surface == "arrow" else layer)
    assert result.crs.equals(CRS.from_user_input(definition))
    assert list(result["id"]) == [1]


@pytest.mark.parametrize("driver", ["gpkg", "fgb", "shp"])
def test_discarded_non_crs_text_is_not_written_into_other_formats(tmp_path, driver):
    path = stored_crs(tmp_path, driver, "not a coordinate reference system")
    with pytest.warns(UserWarning):
        layer = terrio.open_vector(str(path))[0]
    for extension in ["gpkg", "fgb", "shp"]:
        target = tmp_path / f"copy.{extension}"
        layer.write(str(target))
        assert terrio.open_vector(str(target))[0].crs is None


@pytest.mark.parametrize("driver", ["gpkg", "fgb", "shp"])
@pytest.mark.parametrize("surface", ["geopandas", "arrow", "stream", "pyogrio"])
def test_custom_dynamic_crs_keeps_frame_reference_epoch(tmp_path, driver, surface):
    gpd = geopandas_for_surface(surface)
    from pyproj import CRS

    import terrio.pyogrio as tpg

    dynamic = CRS.from_epsg(9000).to_json_dict()
    dynamic.pop("id", None)
    dynamic["name"] = "custom dynamic CRS"
    dynamic["datum"]["name"] = "custom dynamic datum"
    reference = CRS.from_json_dict(dynamic)
    definition = reference.to_wkt()
    # Reserve a large enough FlatBuffer string slot for the longer dynamic WKT;
    # replacing it must not move later header fields or feature offsets.
    container = CUSTOM.replace('"custom"', '"custom' + "x" * 1000 + '"')
    path = stored_crs(tmp_path, driver, definition, container)
    layer = terrio.open_vector(str(path))[0]
    assert layer.crs == definition
    if surface == "pyogrio":
        result = tpg.read_dataframe(path)
    elif surface == "geopandas":
        result = layer.to_geopandas()
    else:
        result = gpd.GeoDataFrame.from_arrow(layer.to_arrow() if surface == "arrow" else layer)
    assert result.crs.equals(reference)
    assert result.crs.to_json_dict()["datum"]["frame_reference_epoch"] == 2010
    assert list(result["id"]) == [1]


@pytest.mark.parametrize("definition", [CUSTOM, 'LOCAL_CS["site grid",UNIT["metre",1]]'])
@pytest.mark.parametrize("surface", ["geopandas", "arrow", "stream", "pyogrio"])
def test_unknown_authority_uses_usable_stored_wkt_without_warning(tmp_path, definition, surface):
    gpd = geopandas_for_surface(surface)
    from pyproj import CRS

    import terrio.pyogrio as tpg

    path = tmp_path / "coded.fgb"
    terrio.Layer.from_geojson(POINTS, name="points", crs="EPSG:32633").write(str(path))
    raw = bytearray(path.read_bytes())
    declared = (32633).to_bytes(4, "little")
    assert raw.count(declared) == 1
    code_slot = raw.index(declared)
    raw[code_slot : code_slot + 4] = (999999).to_bytes(4, "little")
    wkt_slot = raw.index(b"PROJCS[")
    encoded = definition.encode()
    assert len(encoded) < int.from_bytes(raw[wkt_slot - 4 : wkt_slot], "little")
    raw[wkt_slot - 4 : wkt_slot] = len(encoded).to_bytes(4, "little")
    raw[wkt_slot : wkt_slot + len(encoded) + 1] = encoded + b"\0"
    path.write_bytes(raw)
    layer = terrio.open_vector(str(path))[0]
    assert layer.crs == "EPSG:999999"
    assert layer.crs_wkt == definition
    with warnings.catch_warnings():
        warnings.simplefilter("error", UserWarning)
        if surface == "pyogrio":
            result = tpg.read_dataframe(path)
        elif surface == "geopandas":
            result = layer.to_geopandas()
        else:
            result = gpd.GeoDataFrame.from_arrow(layer.to_arrow() if surface == "arrow" else layer)
    assert result.crs.equals(CRS.from_user_input(definition))
    assert list(result["id"]) == [1]
    assert layer.crs == "EPSG:999999"
