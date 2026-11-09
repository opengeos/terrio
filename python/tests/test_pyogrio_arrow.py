"""terrio.pyogrio's Arrow reads of GeoParquet (`read_arrow`, `open_arrow`, and `read_dataframe`
with `use_arrow=True`), against what pyogrio 0.13 returns through GDAL 3.13.3's Parquet driver
(conda-forge `libgdal-arrow-parquet`; pyogrio's wheels have no Parquet driver), quoted below and
measured on the same file."""

import pytest

pa = pytest.importorskip("pyarrow")
gpd = pytest.importorskip("geopandas")

from test_stream import squares  # noqa: E402

import terrio.pyogrio as tpg  # noqa: E402

BOX = (-100.0, -80.0, -90.0, -70.0)


@pytest.fixture
def path(tmp_path):
    p = str(tmp_path / "squares.parquet")
    squares(1000).write(p)
    return p


def test_read_arrow_gives_gdals_meta_and_table(path):
    meta, table = tpg.read_arrow(path)
    # GDAL 3.13.3: {'crs': 'EPSG:4326', 'encoding': 'UTF-8', 'fields': ['i', 'v', 'name', 'day'],
    # 'geometry_type': 'Polygon', 'geometry_name': 'geometry', 'fid_column': 'OGC_FID'}
    assert meta["crs"] == "EPSG:4326"
    assert meta["encoding"] == "UTF-8"
    assert list(meta["fields"]) == ["i", "v", "name", "day"]
    assert (meta["geometry_type"], meta["geometry_name"], meta["fid_column"]) == (
        "Polygon",
        "geometry",
        "OGC_FID",
    )
    # GDAL: ['i', 'v', 'name', 'day', 'geometry'], `i` as the file has it (int32), 1000 rows
    assert table.column_names == ["i", "v", "name", "day", "geometry"]
    assert table.schema.field("i").type == pa.int32()
    assert table.num_rows == 1000


def test_read_arrow_selects_columns_rows_and_ranges(path):
    meta, table = tpg.read_arrow(path, bbox=BOX, columns=["name"])
    assert table.column_names == ["name", "geometry"]
    assert table.num_rows == 44  # GDAL: 44
    # Unlike GDAL's Arrow stream, whose `skip_features` skips the file's rows before the
    # rectangle filters them (f70..f74 here), the range is taken from the rows the rectangle
    # keeps, as `read_dataframe` takes it.
    _, page = tpg.read_arrow(path, bbox=BOX, columns=["name"], skip_features=3, max_features=5)
    assert page["name"].to_pylist() == table["name"].to_pylist()[3:8]
    _, flat = tpg.read_arrow(path, columns=["v"], read_geometry=False)
    assert flat.column_names == ["v"]


def test_open_arrow_streams_batches(path):
    with tpg.open_arrow(path, batch_size=300, use_pyarrow=True) as (meta, reader):
        sizes = [b.num_rows for b in reader]
    assert sizes == [300, 300, 300, 100]
    assert meta["geometry_name"] == "geometry"
    # as pyogrio refuses them
    with pytest.raises(ValueError, match="max_features"):
        with tpg.open_arrow(path, max_features=3):
            pass


def test_read_dataframe_with_arrow_matches_gdal(path):
    gdf = tpg.read_dataframe(path, use_arrow=True, bbox=BOX)
    # GDAL: ['i', 'v', 'name', 'day', 'geometry'], 44 rows, EPSG:4326, `i` int32
    assert list(gdf.columns) == ["i", "v", "name", "day", "geometry"]
    assert len(gdf) == 44
    assert gdf.crs.to_string() == "EPSG:4326"
    assert str(gdf["i"].dtype) == "int32"
    regular = tpg.read_dataframe(path, bbox=BOX)
    assert gdf.geometry.equals(regular.geometry)
    assert gdf["name"].tolist() == regular["name"].tolist()


def test_arrow_reads_of_other_formats_are_not_provided(tmp_path):
    p = str(tmp_path / "s.geojson")
    squares(10).write(p)
    with pytest.raises(NotImplementedError, match="GeoParquet"):
        tpg.read_arrow(p)
