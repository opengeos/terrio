"""Stored CRS robustness across vector readers and dataframe/Arrow exports (#784)."""

import sqlite3

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


def stored_crs(tmp_path, driver, text):
    path = tmp_path / f"points.{driver}"
    layer = terrio.Layer.from_geojson(POINTS, name="points", crs=CUSTOM)
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
        start = raw.index(b'PROJCS["custom"')
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
    gpd = pytest.importorskip("geopandas")
    pytest.importorskip("pyarrow")
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
    gpd = pytest.importorskip("geopandas")
    pytest.importorskip("pyarrow")
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
