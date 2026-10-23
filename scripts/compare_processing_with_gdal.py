"""Compare the processing commands with GDAL/OGR on real-world data.

Run in an environment with GDAL's Python bindings and NumPy::

    conda run -n geo python scripts/compare_processing_with_gdal.py ../terrio-data

Centroids are compared with OGR/GEOS, singleparts and vertices with OGR's geometry traversal,
footprints with GDAL's full-resolution footprint (without simplification), and multi-raster
sampling with GDAL's coordinate transformations, inverse affine transforms and mask bands.
Every mismatch raises an error. Outputs are temporary; the input datasets are never modified.
"""

from __future__ import annotations

import argparse
import json
import math
import subprocess
import tempfile
from pathlib import Path

from osgeo import gdal, ogr, osr

gdal.UseExceptions()
ogr.UseExceptions()
osr.UseExceptions()


def run(terrio: Path, command: str, source: Path, output: Path, *args: str) -> None:
    result = subprocess.run(
        [str(terrio), command, str(source), str(output), *args],
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode:
        raise RuntimeError(f"{command}: {result.stderr}")


def features(path: Path) -> list:
    ds = ogr.Open(str(path))
    # Feature clones own their geometries after the dataset is closed.
    return [f.Clone() for f in ds.GetLayer(0)]


def parts(g):
    if g is None:
        yield None
    elif ogr.GT_Flatten(g.GetGeometryType()) in (
        ogr.wkbPoint,
        ogr.wkbLineString,
        ogr.wkbPolygon,
    ):
        yield g
    else:
        for i in range(g.GetGeometryCount()):
            yield from parts(g.GetGeometryRef(i))


def attributes(feature) -> dict:
    # OGR exposes list fields as lists and some JSON fields as text; GeoPackage represents
    # both as JSON text. Compare their decoded values, independent of whitespace.
    def value(v):
        if isinstance(v, str) and v.startswith(("[", "{")):
            try:
                return json.loads(v)
            except ValueError:
                pass
        return v

    return {k: value(v) for k, v in feature.items().items()}


def vertices(g):
    for part, component in enumerate(parts(g)):
        if component is None:
            continue
        if ogr.GT_Flatten(component.GetGeometryType()) == ogr.wkbPolygon:
            rings = [
                (i, component.GetGeometryRef(i))
                for i in range(component.GetGeometryCount())
            ]
        else:
            rings = [(None, component)]
        for ring, line in rings:
            for vertex in range(line.GetPointCount()):
                yield part, ring, vertex, line.GetPoint(vertex)


def compare_geometry(terrio: Path, source: Path, work: Path) -> None:
    original = features(source)
    for command in ("centroid", "singlepart", "vertices"):
        out = work / f"{source.stem}-{command}.gpkg"
        run(terrio, command, source, out)
        result = features(out)
        if command == "centroid":
            assert len(result) == len(original)
            error = 0.0
            for a, b in zip(original, result):
                expected = a.GetGeometryRef().Centroid()
                got = b.GetGeometryRef()
                error = max(
                    error,
                    math.hypot(
                        expected.GetX() - got.GetX(), expected.GetY() - got.GetY()
                    ),
                )
                assert attributes(a) == attributes(b), (source.name, a.GetFID())
            assert error < 1e-8, error
            detail = f"max coordinate error {error:.3g}"
        elif command == "singlepart":
            expected = [
                (attributes(a), g) for a in original for g in parts(a.GetGeometryRef())
            ]
            assert len(result) == len(expected)
            ids = [f.GetFID() for f in result]
            assert len(set(ids)) == len(ids)
            for b, (attrs, g) in zip(result, expected):
                assert attributes(b) == attrs
                assert b.GetGeometryRef().Equals(g)
            detail = "geometries and attributes identical"
        else:
            expected = [
                (attrs, a.GetFID(), v)
                for a in original
                for attrs in [attributes(a)]
                for v in vertices(a.GetGeometryRef())
            ]
            assert len(result) == len(expected)
            for b, (attrs, fid, (part, ring, vertex, point)) in zip(result, expected):
                got = b.GetGeometryRef()
                assert (got.GetX(), got.GetY()) == point[:2]
                assert b["source_fid"] == str(fid)
                assert (b["part_index"], b["ring_index"], b["vertex_index"]) == (
                    part,
                    ring,
                    vertex,
                )
                actual = attributes(b)
                assert {k: actual[k] for k in attrs} == attrs
            detail = "coordinates, attributes and provenance identical"
        print(f"PASS {source.name} {command}: {len(result)} features; {detail}")


def compare_footprint(terrio: Path, source: Path, work: Path) -> None:
    for intersection in (False, True):
        out = work / f"{source.stem}-footprint-{intersection}.gpkg"
        run(
            terrio,
            "footprint",
            source,
            out,
            *(["--intersection"] if intersection else []),
        )
        got = features(out)
        ds = gdal.Footprint(
            "",
            str(source),
            format="MEM",
            simplify=0,
            maxPoints="unlimited",
            combineBands="intersection" if intersection else "union",
        )
        feature = ds.GetLayer(0).GetNextFeature()
        expected = feature.GetGeometryRef().Clone()
        assert len(got) == 1
        actual = got[0].GetGeometryRef()
        assert actual.IsValid()
        difference = actual.SymDifference(expected).GetArea()
        tolerance = max(1e-6, expected.GetArea() * 1e-12)
        assert difference <= tolerance, (source.name, difference, tolerance)
        print(
            f"PASS {source.name} footprint {'intersection' if intersection else 'union'}: difference area {difference:.3g}"
        )


def traditional(srs):
    srs = srs.Clone()
    srs.SetAxisMappingStrategy(osr.OAMS_TRADITIONAL_GIS_ORDER)
    return srs


def compare_sample(terrio: Path, sources: list[Path], work: Path) -> None:
    rasters = [gdal.Open(str(p)) for p in sources]
    wgs84 = osr.SpatialReference()
    wgs84.ImportFromEPSG(4326)
    wgs84 = traditional(wgs84)
    points = []
    for raster in rasters:
        ct = osr.CoordinateTransformation(traditional(raster.GetSpatialRef()), wgs84)
        for row in range(0, raster.RasterYSize, max(1, raster.RasterYSize // 12)):
            for col in range(0, raster.RasterXSize, max(1, raster.RasterXSize // 12)):
                x, y = gdal.ApplyGeoTransform(
                    raster.GetGeoTransform(), col + 0.5, row + 0.5
                )
                lon, lat, _ = ct.TransformPoint(x, y)
                points.append([lon, lat])
    points.append([0.0, 0.0])
    source = work / "sites.geojson"
    source.write_text(
        json.dumps(
            {
                "type": "FeatureCollection",
                "features": [
                    {
                        "type": "Feature",
                        "properties": {"site": i},
                        "geometry": {"type": "Point", "coordinates": p},
                    }
                    for i, p in enumerate(points)
                ],
            }
        )
    )
    output = work / "samples.gpkg"
    args = [
        arg
        for i, p in enumerate(sources)
        for arg in ("--raster", str(p), "--prefix", f"r{i}")
    ]
    run(terrio, "sample", source, output, *args)
    result = features(output)
    assert len(result) == len(points)
    valid_count = 0
    for i, raster in enumerate(rasters):
        ct = osr.CoordinateTransformation(wgs84, traditional(raster.GetSpatialRef()))
        inverse = gdal.InvGeoTransform(raster.GetGeoTransform())
        band = raster.GetRasterBand(1)
        for j, (lon, lat) in enumerate(points):
            x, y, _ = ct.TransformPoint(lon, lat)
            col, row = gdal.ApplyGeoTransform(inverse, x, y)
            col, row = math.floor(col), math.floor(row)
            expected = None
            if 0 <= col < raster.RasterXSize and 0 <= row < raster.RasterYSize:
                v = float(band.ReadAsArray(col, row, 1, 1)[0, 0])
                if (
                    math.isfinite(v)
                    and band.GetMaskBand().ReadAsArray(col, row, 1, 1)[0, 0]
                ):
                    expected = v
            got = result[j][f"r{i}_b1"]
            assert got == expected, (i, j, got, expected)
            assert result[j]["site"] == j
            g = result[j].GetGeometryRef()
            assert (g.GetX(), g.GetY()) == (lon, lat)
            valid_count += expected is not None
    assert valid_count > 0
    print(
        f"PASS multi-raster sampling: {len(points)} points, {len(rasters)} rasters, {valid_count} valid samples; values/nulls identical"
    )


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data", type=Path)
    parser.add_argument("--terrio", type=Path, default=Path("target/debug/terrio"))
    args = parser.parse_args()
    terrio, data = args.terrio.resolve(), args.data.resolve()
    print(f"GDAL {gdal.VersionInfo('--version')}")
    with tempfile.TemporaryDirectory(prefix="terrio-processing-") as tmp:
        work = Path(tmp)
        for name in ("countries.geojson", "cables.geojson", "TN_Counties.geojson"):
            compare_geometry(terrio, data / name, work)
        for name in ("dem.tif", "landsat.tif"):
            compare_footprint(terrio, data / name, work)
        compare_sample(
            terrio,
            [data / name for name in ("dem.tif", "landsat.tif", "nlcd_knoxville.tif")],
            work,
        )


if __name__ == "__main__":
    main()
