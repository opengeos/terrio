"""Tests of the HECRAS vector driver (HEC-RAS geometry and plan HDF files) through the Python
API: detection by content, layers, the stored CRS, and joins by open options. Uses the synthetic
plan `crates/terrio/tests/data/hecras/results_mesh_xs.h5` (`scripts/make_hecras_fixture.py`)."""

from pathlib import Path

import pytest

import terrio

DATA = Path(__file__).resolve().parents[2] / "crates" / "terrio" / "tests" / "data" / "hecras"
PLAN = str(DATA / "results_mesh_xs.h5")

pytestmark = pytest.mark.skipif(
    "HECRAS" not in {d["name"] for d in terrio.drivers()}, reason="built without the hecras feature"
)


def test_plans_open_as_vector_layers():
    ds = terrio.open_vector(PLAN)
    assert ds.driver == "HECRAS"
    assert ds.layer_names[:3] == ["mesh_cells", "mesh_cell_points", "mesh_faces"]
    assert "cross_sections" in ds.layer_names
    cells = ds["mesh_cells"]
    assert cells.geometry_type == "Polygon"
    assert list(cells.fields)[:2] == ["mesh_name", "cell_id"]
    # The stored projection text, with no EPSG code guessed.
    assert cells.crs_wkt.startswith('PROJCS["Synthetic"')
    # `open` reads it as vector layers too.
    assert isinstance(terrio.open(PLAN), terrio.VectorDataset)


def test_open_options_join_stored_values():
    ds = terrio.open_vector(
        PLAN,
        layers=["mesh_cells"],
        options={
            "CELL_SUMMARIES": "Maximum Water Surface",
            "CELL_SERIES": "Water Surface",
            "TIME_STEP": "1",
        },
    )
    cells = ds["mesh_cells"]
    assert {"maximum_water_surface", "maximum_water_surface_time", "water_surface"} <= set(
        cells.fields
    )
    assert cells.columns()["water_surface"] == [12.75, 12.25]


def test_feature_layers_at_a_time_step():
    ds = terrio.open_vector(
        PLAN,
        layers=["cross_sections", "bc_lines"],
        options={"XS_RESULTS": "Water Surface", "BC_SERIES": "YES", "TIME_STEP": "2"},
    )
    assert ds["cross_sections"].columns()["water_surface"] == [21.0, 19.0, 15.5]
    assert ds["bc_lines"].columns()["stage"] == [12.25, 11.25]


def test_info_takes_open_options():
    summary = terrio.info(
        PLAN, layers=["mesh_cells"], options={"CELL_ATTRIBUTES": "MINIMUM_ELEVATION"}
    )
    assert summary["driver"] == "HECRAS"
    layer = summary["layers"][0]
    assert layer["name"] == "mesh_cells"
    assert [f["name"] for f in layer["fields"]] == ["mesh_name", "cell_id", "minimum_elevation"]
    assert layer["fields"][2]["field_type"] == "Float"


def test_info_with_empty_options_keeps_the_raster_summary(tmp_path):
    import numpy as np

    path = str(tmp_path / "small.tif")
    terrio.Raster.from_array(np.zeros((1, 2, 2), dtype="uint8"), (0, 1, 0, 2, 0, -1)).write(path)
    assert terrio.info(path, options={})["driver"] == terrio.info(path)["driver"]


def test_misuse_is_an_error():
    # A series without TIME_STEP, and a value the file does not store: errors, never empty layers.
    with pytest.raises(terrio.TerrioError):
        terrio.open_vector(PLAN, layers=["mesh_cells"], options={"CELL_SERIES": "Water Surface"})
    with pytest.raises(terrio.TerrioError):
        terrio.open_vector(
            PLAN, layers=["mesh_cells"], options={"CELL_SERIES": "Depth", "TIME_STEP": "0"}
        )
    with pytest.raises(terrio.TerrioError):
        terrio.open_vector(PLAN, options={"NO_SUCH_OPTION": "1"})
    with pytest.raises(terrio.TerrioError):
        terrio.open_vector(PLAN, layers=["structures"])


def test_raster_opens_keep_reading_hdf5():
    # A raster open of a HEC-RAS file still goes to the HDF5 raster driver, as before: it reads
    # one of the file's arrays, never through the HECRAS driver.
    r = terrio.open_raster(PLAN)
    assert isinstance(r, terrio.Raster)
    assert "driver=HDF5" in repr(r)


def test_left_out_layers_are_user_warnings():
    # A default open leaves out the layers the engine refuses (here: above the array budget),
    # and says so with a UserWarning, from open_vector and info alike.
    options = {"MAX_BYTES": "256"}
    with pytest.warns(UserWarning, match="layer mesh_cells left out"):
        ds = terrio.open_vector(PLAN, options=options)
    assert "mesh_cells" not in ds.layer_names
    assert "cross_sections" in ds.layer_names
    with pytest.warns(UserWarning, match="layer mesh_faces left out"):
        summary = terrio.info(PLAN, options=options)
    assert summary["driver"] == "HECRAS"
    with pytest.warns(UserWarning, match="left out"), pytest.raises(terrio.TerrioError):
        # Every layer refused: the warnings are issued and the open fails.
        terrio.open_vector(PLAN, options={"MAX_BYTES": "1"})


def test_invalid_requests_fail_without_naming_layers():
    # An incomplete request is an error, never a dataset without the layers it concerns.
    with pytest.raises(terrio.TerrioError, match="TIME_STEP"):
        terrio.open_vector(PLAN, options={"CELL_SERIES": "Water Surface"})
    with pytest.raises(terrio.TerrioError, match="TIME_STEP"):
        terrio.open_vector(PLAN, options={"XS_RESULTS": "Water Surface"})


def test_options_for_layers_the_file_does_not_store_are_errors():
    pipes = str(DATA / "pipes.h5")
    with pytest.raises(terrio.InvalidArgumentError, match="XS_SUMMARIES applies to cross_sections"):
        terrio.open_vector(pipes, options={"XS_SUMMARIES": "Maximum Water Surface"})
    with pytest.raises(terrio.InvalidArgumentError, match="STRUCTURE_SERIES applies to structures"):
        terrio.open_vector(
            PLAN, options={"STRUCTURE_SERIES": "Structure Variables", "TIME_STEP": "0"}
        )
    with pytest.raises(terrio.InvalidArgumentError, match="CELL_SERIES"):
        terrio.info(pipes, options={"CELL_SERIES": "Water Surface", "TIME_STEP": "0"})


def test_root_attributes_keep_to_the_metadata_budget():
    budget = {"METADATA_MAX_BYTES": "4096", "MAX_BYTES": "4096"}
    for name in ("large_projection.h5", "large_root_attribute.h5"):
        path = str(DATA / name)
        with pytest.raises(terrio.UnsupportedError, match="root attributes"):
            terrio.open_vector(path, options=budget)
        assert terrio.open_vector(path).driver == "HECRAS"
