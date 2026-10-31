"""Pipelines share the core engine and defer explicit outputs until requested."""

import json

import numpy as np
import pytest

import terrio


def points():
    """Return two attributed points in a named layer."""
    return terrio.Layer.from_geojson(
        {
            "type": "FeatureCollection",
            "features": [
                {
                    "type": "Feature",
                    "properties": {"score": score, "name": f"point-{score}"},
                    "geometry": {"type": "Point", "coordinates": [score, 1]},
                }
                for score in [0, 3]
            ],
        },
        name="points",
    )


def test_vector_aliases_deferred_bytes_and_source_immutability(tmp_path):
    """A composed vector result retains geometry when explicitly selected."""
    layer = points()
    before = layer.to_geojson()
    output = tmp_path / "selected.geojson"
    result = terrio.pipeline(
        f"read source ! filter --where 'score > 1' ! select name,_ogr_geometry_ "
        f"! buffer 1 ! materialize ! write '{output}'",
        {"source": layer},
    )
    assert not output.exists()
    assert result.output_names == [str(output)]
    assert isinstance(result.dataset, terrio.VectorDataset)
    assert len(result.dataset.layers[0]) == 1
    assert layer.to_geojson() == before
    content = result.output_bytes()[str(output)]
    assert len(json.loads(content)["features"]) == 1
    assert not output.exists()
    result.write_outputs()
    assert output.read_bytes() == content
    with pytest.raises(ValueError, match="exists"):
        result.write_outputs()


def test_attribute_selection_and_geometry_selection():
    """Geometry participates in field selection just as in a GDAL pipeline."""
    layer = points()
    table = terrio.pipeline("read source ! select name", {"source": layer}).dataset.layers[0]
    assert all(f["geometry"] is None for f in table.to_geojson()["features"])
    spatial = terrio.pipeline(
        "read source ! select name,_ogr_geometry_", {"source": layer}
    ).dataset.layers[0]
    assert all(f["geometry"] is not None for f in spatial.to_geojson()["features"])


def test_exact_uint64_aliases_and_explicit_writes(tmp_path):
    """Neither binding nor view steps round UInt64 through floating point."""
    values = np.array([[2**53 + 1, 2**53 + 3], [2**64 - 2, 1]], dtype=np.uint64)
    source = terrio.Raster.from_array(values, [0, 1, 0, 2, 0, -1], "EPSG:4326")
    path = tmp_path / "copy.tif"
    result = terrio.pipeline(
        f"read image ! select 1 ! materialize ! write '{path}'", {"image": source}
    )
    np.testing.assert_array_equal(result.dataset.read(1), values)
    assert not path.exists()
    terrio.pipeline(f"read image ! write '{path}'", {"image": source}, write=True)
    np.testing.assert_array_equal(terrio.open_raster(str(path)).read(1), values)
    np.testing.assert_array_equal(source.read(1), values)
    assert result.output_bytes()[str(path)].startswith(b"II")


def test_saved_definition_loading_relative_paths_and_no_evaluation(tmp_path):
    """Definitions can be saved without opening a missing source."""
    saved = terrio.pipeline("read missing.tif ! select 1 ! write saved.gdalg.json")
    assert saved.dataset is None
    definition = json.loads(saved.output_bytes()["saved.gdalg.json"])
    assert definition["type"] == "gdal_streamed_alg"
    layer = points()
    source = tmp_path / "input.geojson"
    layer.write(str(source))
    definition["command_line"] = (
        "gdal vector pipeline read input.geojson ! filter --where 'score > 1'"
    )
    definition["relative_paths_relative_to_this_file"] = True
    result = terrio.pipeline(json.dumps(definition), base_file=str(tmp_path / "saved.gdalg.json"))
    assert len(result.dataset.layers[0]) == 1
    alias = terrio.pipeline("read native ! filter --where 'score > 1'", {"native": source})
    assert len(alias.dataset.layers[0]) == 1


def test_checkpoint_preflight_and_parser_guards(tmp_path):
    """No output is modified if any requested destination fails preflight."""
    checkpoint, protected = tmp_path / "checkpoint.geojson", tmp_path / "protected.geojson"
    protected.write_bytes(b"preserve")
    result = terrio.pipeline(
        f"read a ! materialize --output '{checkpoint}' ! write '{protected}'", {"a": points()}
    )
    with pytest.raises(ValueError, match="exists"):
        result.write_outputs()
    assert not checkpoint.exists()
    assert protected.read_bytes() == b"preserve"
    with pytest.raises(ValueError, match="unsupported"):
        terrio.pipeline("read a ! external echo hello", {"a": points()})


def test_remote_crop_keeps_window_overview_and_mask_reads_bounded(tmp_path):
    """A crop view fetches a small fraction of a real tiled TIFF's bytes."""
    import threading
    from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

    gdal = pytest.importorskip("osgeo.gdal")
    gdal.UseExceptions()
    source = tmp_path / "source.tif"
    output = tmp_path / "source-cog.tif"
    values = np.arange(1024 * 1024, dtype=np.uint16).reshape(1024, 1024)
    ds = gdal.GetDriverByName("GTiff").Create(str(source), 1024, 1024, 1, gdal.GDT_UInt16)
    ds.SetGeoTransform([0, 1, 0, 1024, 0, -1])
    ds.SetProjection("EPSG:3857")
    ds.GetRasterBand(1).WriteArray(values)
    ds.CreateMaskBand(gdal.GMF_PER_DATASET)
    mask = np.full((1024, 1024), 255, dtype=np.uint8)
    mask[128:144, 128:144] = 0
    ds.GetRasterBand(1).GetMaskBand().WriteArray(mask)
    ds = None
    ds = gdal.Translate(
        str(output), str(source), format="COG", creationOptions=["COMPRESS=NONE", "BLOCKSIZE=128"]
    )
    expected_overview = ds.GetRasterBand(1).GetOverview(0).ReadAsArray(64, 64, 4, 4)
    expected_mask = ds.GetRasterBand(1).GetOverview(0).GetMaskBand().ReadAsArray(64, 64, 4, 4)
    ds = None
    payload = output.read_bytes()
    counts = {"bytes": 0, "gets": 0, "heads": 0, "paths": set()}

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_HEAD(self):
            counts["heads"] += 1
            if self.path != "/source.tif":
                self.send_response(404)
                self.send_header("Content-Length", "0")
                self.end_headers()
                return
            self.send_response(200)
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()

        def do_GET(self):
            counts["gets"] += 1
            if self.path != "/source.tif":
                self.send_response(404)
                self.send_header("Content-Length", "0")
                self.end_headers()
                return
            span = self.headers.get("Range")
            first, last = 0, len(payload) - 1
            if span:
                first, end = span.removeprefix("bytes=").split("-", 1)
                first, last = int(first), min(int(end) if end else last, last)
            block = payload[first : last + 1]
            counts["bytes"] += len(block)
            counts["paths"].add(self.path)
            self.send_response(206 if span else 200)
            self.send_header("Content-Length", str(len(block)))
            self.send_header("Content-Range", f"bytes {first}-{last}/{len(payload)}")
            self.end_headers()
            self.wfile.write(block)

    with ThreadingHTTPServer(("127.0.0.1", 0), Handler) as server:
        worker = threading.Thread(target=server.serve_forever, daemon=True)
        worker.start()
        try:
            url = f"http://127.0.0.1:{server.server_port}/source.tif"
            result = terrio.pipeline(f"read {url} ! clip --bbox 128,768,256,896 ! select 1")
            raster = result.dataset
            assert raster.is_lazy
            assert raster.overviews
            np.testing.assert_array_equal(
                raster.read(1, window=(0, 0, 8, 8)), values[128:136, 128:136]
            )
            np.testing.assert_array_equal(
                raster.read_mask(window=(0, 0, 8, 8)), mask[128:136, 128:136]
            )
            np.testing.assert_array_equal(
                raster.read(1, window=(0, 0, 4, 4), overview=0), expected_overview
            )
            np.testing.assert_array_equal(
                raster.read_mask(window=(0, 0, 4, 4), overview=0), expected_mask
            )
            assert counts["paths"] == {"/source.tif"}
            assert 0 < counts["bytes"] < len(payload) // 4, counts
            print(
                f"pipeline remote I/O: {counts['gets']} GETs, {counts['heads']} HEADs, "
                f"{counts['bytes']}/{len(payload)} bytes"
            )
        finally:
            server.shutdown()
            worker.join()


def test_dissolve_unions_components_within_each_feature():
    """Unary dissolve preserves rows and attributes while noding and merging."""
    geometries = [
        {"type": "MultiLineString", "coordinates": [[[0, 0], [4, 0]], [[2, 0], [6, 0]]]},
        {"type": "MultiLineString", "coordinates": [[[0, 0], [4, 4]], [[0, 4], [4, 0]]]},
        {"type": "MultiPoint", "coordinates": [[1, 1], [1, 1], [2, 2]]},
        {
            "type": "MultiPolygon",
            "coordinates": [
                [[[0, 0], [2, 0], [2, 2], [0, 2], [0, 0]]],
                [[[1, 0], [3, 0], [3, 2], [1, 2], [1, 0]]],
            ],
        },
        {"type": "MultiPoint", "coordinates": []},
        None,
    ]
    layer = terrio.Layer.from_geojson(
        {
            "type": "FeatureCollection",
            "features": [
                {"type": "Feature", "properties": {"id": i}, "geometry": geometry}
                for i, geometry in enumerate(geometries)
            ],
        }
    )
    before = layer.to_geojson()
    result = terrio.pipeline("read source ! dissolve", {"source": layer}).dataset.layers[0]
    features = result.to_geojson()["features"]
    assert [f["properties"] for f in features] == [{"id": i} for i in range(len(geometries))]
    assert features[0]["geometry"]["type"] == "LineString"
    assert len(features[0]["geometry"]["coordinates"]) == 4
    assert features[1]["geometry"]["type"] == "MultiLineString"
    assert len(features[1]["geometry"]["coordinates"]) == 4
    assert len(features[2]["geometry"]["coordinates"]) == 2
    assert features[3]["geometry"]["type"] == "Polygon"
    assert features[4]["geometry"] == {"type": "GeometryCollection", "geometries": []}
    assert features[5]["geometry"] is None
    assert layer.to_geojson() == before
