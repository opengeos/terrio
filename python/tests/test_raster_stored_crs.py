"""Stored raster CRS definitions take precedence over authority-code summaries."""

import json
import sqlite3

import numpy as np
import pytest

import terrio

TRANSFORM = (1000.0, 10.0, 0.0, 2000.0, 0.0, -10.0)
VALUES = np.arange(12, dtype="uint8").reshape(1, 3, 4)
ENGINEERING = [
    'LOCAL_CS["site",LOCAL_DATUM["site",32767],UNIT["metre",1],'
    'AXIS["northing",NORTH],AXIS["westing",WEST]]',
    'ENGCRS["site",EDATUM["site"],CS[Cartesian,2],AXIS["northing",north,ORDER[1]],'
    'AXIS["westing",west,ORDER[2]],LENGTHUNIT["metre",1]]',
    'ENGINEERINGCRS["site",ENGINEERINGDATUM["site"],CS[Cartesian,2],'
    'AXIS["northing",north,ORDER[1]],AXIS["westing",west,ORDER[2]],LENGTHUNIT["metre",1]]',
]


def update_zarr_attributes(path, attributes):
    """Update both array attributes and the consolidated metadata used by readers.

    Args:
        path: Zarr store directory.
        attributes: Replacement attributes for the site array.
    """
    (path / "site" / ".zattrs").write_text(json.dumps(attributes))
    consolidated = path / ".zmetadata"
    metadata = json.loads(consolidated.read_text())
    assert "site/.zattrs" in metadata["metadata"]
    metadata["metadata"]["site/.zattrs"] = attributes
    consolidated.write_text(json.dumps(metadata))


@pytest.mark.parametrize("extension", ["zarr", "gpkg"])
@pytest.mark.parametrize("identifier_fallback", [False, True])
def test_stored_crs_text_does_not_resolve_a_local_filename(
    tmp_path, extension, identifier_fallback
):
    """Keep external filenames out of CRS metadata parsing.

    Args:
        tmp_path: Temporary dataset directory.
        extension: Format whose stored definition can be edited independently.
        identifier_fallback: Retain a valid EPSG identifier alongside the filename.
    """
    external = tmp_path / "unrelated.wkt"
    external.write_text("EPSG:4326")
    source = terrio.Raster.from_array(VALUES, TRANSFORM, crs="EPSG:32615")
    path = tmp_path / ("site." + extension)
    source.write(str(path))
    if extension == "gpkg":
        with sqlite3.connect(path) as db:
            db.execute(
                "UPDATE gpkg_spatial_ref_sys SET definition = ? WHERE srs_id = 32615",
                (str(external),),
            )
            if not identifier_fallback:
                db.execute(
                    "UPDATE gpkg_spatial_ref_sys SET organization = 'NONE', "
                    "organization_coordsys_id = 0 WHERE srs_id = 32615"
                )
    else:
        attributes_path = path / "site" / ".zattrs"
        attributes = json.loads(attributes_path.read_text())
        attributes["_CRS"] = {"wkt": str(external)}
        if identifier_fallback:
            attributes["_CRS"]["url"] = "http://www.opengis.net/def/crs/EPSG/0/32615"
        update_zarr_attributes(path, attributes)
    result = terrio.open_raster(str(path))
    assert result.crs == ("EPSG:32615" if identifier_fallback else None)
    assert result.crs_wkt == (source.crs_wkt if identifier_fallback else None)
    assert tuple(result.transform) == TRANSFORM
    np.testing.assert_array_equal(np.asarray(result), VALUES)


@pytest.mark.parametrize("extension", ["jp2", "zarr", "gpkg"])
@pytest.mark.parametrize("definition", ENGINEERING, ids=["local", "eng", "engineering"])
@pytest.mark.parametrize("authority", [None, "EPSG:5800", "EPSG:999999", "IGNF:SITEGRID"])
def test_stored_engineering_definition_survives_raster_roundtrip(
    tmp_path, extension, definition, authority
):
    """Read the definition actually stored, including unknown authority identifiers.

    Args:
        tmp_path: Temporary output directory.
        extension: Raster storage format.
        definition: Engineering WKT spelling.
        authority: Optional root identifier, independently of the WKT definition.
    """
    if authority:
        name, code = authority.split(":")
        identifier = (
            f'AUTHORITY["{name}","{code}"]'
            if definition.startswith("LOCAL_CS")
            else f'ID["{name}",{code if code.isdigit() else json.dumps(code)}]'
        )
        definition = definition[:-1] + "," + identifier + "]"
    source = terrio.Raster.from_array(VALUES, TRANSFORM, crs=definition)
    path = tmp_path / ("site." + extension)
    source.write(str(path))
    # Inspect storage independently so a writer and reader cannot hide each other's loss.
    if extension == "gpkg":
        with sqlite3.connect(path) as db:
            assert definition in {
                row[0] for row in db.execute("SELECT definition FROM gpkg_spatial_ref_sys")
            }
    elif extension == "zarr":
        attributes = json.loads((path / "site" / ".zattrs").read_text())
        assert attributes["_CRS"]["wkt"] == definition
    else:
        assert definition.encode() in path.read_bytes()
    for lazy in (False, True):
        result = terrio.open_raster(str(path), lazy=lazy)
        assert result.crs_wkt == definition
        assert tuple(result.transform) == TRANSFORM
        samples = np.asarray(result)
        assert samples.dtype == VALUES.dtype
        np.testing.assert_array_equal(samples, VALUES)
        with pytest.raises(terrio.CRSError):
            terrio.warp(result, t_srs="EPSG:4326")


@pytest.mark.parametrize("extension", ["jp2", "zarr", "gpkg"])
def test_registered_raster_crs_keeps_definition_and_values(tmp_path, extension):
    """Preserve registered horizontal CRS behavior alongside definition-first reading.

    Args:
        tmp_path: Temporary directory.
        extension: Raster storage format.
    """
    source = terrio.Raster.from_array(VALUES, TRANSFORM, crs="EPSG:32615")
    path = tmp_path / ("site." + extension)
    source.write(str(path))
    result = terrio.open_raster(str(path))
    assert result.crs == "EPSG:32615"
    assert result.crs_wkt == source.crs_wkt
    assert tuple(result.transform) == TRANSFORM
    np.testing.assert_array_equal(np.asarray(result), VALUES)
    # Missing definitions still fall back to the stored registered authority code.
    if extension == "gpkg":
        with sqlite3.connect(path) as db:
            db.execute("UPDATE gpkg_spatial_ref_sys SET definition = 'undefined'")
    elif extension == "zarr":
        attributes_path = path / "site" / ".zattrs"
        attributes = json.loads(attributes_path.read_text())
        attributes["_CRS"]["wkt"] = ""
        update_zarr_attributes(path, attributes)
    else:
        return
    result = terrio.open_raster(str(path))
    assert result.crs == "EPSG:32615"
    assert result.crs_wkt == source.crs_wkt
    np.testing.assert_array_equal(np.asarray(result), VALUES)
