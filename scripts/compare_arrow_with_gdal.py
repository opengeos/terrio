"""Compare terrio's Arrow export and import of vector layers with GDAL, pyarrow and GeoPandas.

For each layer of the real-world files of ``scripts/fetch_test_data.sh``:

* export: ``Layer.to_arrow()`` (WKB, and GeoArrow native encodings where the layer has one
  geometry type), read by pyarrow through the Arrow PyCapsule interface, against GDAL's Arrow
  stream of the same layer (``ogr.Layer.GetArrowStreamAsPyArrow()``, with WKB and with GeoArrow
  encodings): row count, geometries (WKB-equal once parsed by Shapely), attribute values by name,
  and the CRS in the GeoArrow metadata;
* GeoPandas: ``GeoDataFrame.from_arrow()`` of terrio's table against that of GDAL's (geometries,
  CRS, attributes);
* import: ``terrio.from_arrow()`` of GDAL's stream (WKB and native), of GeoPandas'
  ``to_arrow()`` (WKB and native) and of the ``ogr.Layer`` itself, against the layer terrio reads
  from the file;
* GDAL import: ``ogr.Layer.WriteArrow()`` of terrio's table into a GeoPackage, read back by GDAL.

Needs GDAL's Python bindings with pyarrow, GeoPandas and Shapely, and terrio's Python package
(``maturin develop``, or ``PYTHONPATH=python/python`` after building it).

Usage::

    python scripts/compare_arrow_with_gdal.py ../terrio-data
"""

from __future__ import annotations

import argparse
import datetime as dt
import math
import sys
from pathlib import Path

import geopandas as gpd
import numpy as np
import pyarrow as pa
import shapely
from osgeo import gdal, ogr, osr

import terrio

gdal.UseExceptions()
ogr.UseExceptions()

GEOARROW_TYPES = [
    "wkb",
    "wkt",
    "point",
    "linestring",
    "polygon",
    "multipoint",
    "multilinestring",
    "multipolygon",
]


def plain_geoarrow() -> None:
    """Unregisters the GeoArrow extension types.

    pyarrow then keeps GeoArrow columns as their storage types, with the extension name and
    metadata in the field metadata, as compared here.

    Arrow C++ registers ``geoarrow.wkb``, and GeoPandas and geoarrow-pyarrow register the others
    when they are used.
    """
    for name in GEOARROW_TYPES:
        try:
            pa.unregister_extension_type(f"geoarrow.{name}")
        except (KeyError, pa.ArrowKeyError):
            pass


def to_table(obj) -> pa.Table:
    """A pyarrow table of an object exporting Arrow data, GeoArrow columns kept as storage types.

    Args:
        obj: An object with ``__arrow_c_stream__``.

    Returns:
        The table.
    """
    plain_geoarrow()
    return pa.table(obj)


# (file, layers): None for all layers.
DATASETS: list[tuple[str, list[str] | None]] = [
    ("countries.gpkg", None),
    ("UScounties.gpkg", None),
    ("UScounties.fgb", None),
    ("ne_places.zip", None),
    ("nybb.zip", None),
    ("countries.parquet", None),
    ("las_vegas_roads.geojson", None),
    ("hike.gpx", None),
    ("liechtenstein.osm.pbf", ["points", "lines", "multilinestrings", "multipolygons"]),
]


def gdal_path(path: Path) -> str:
    """The path GDAL opens a file by (zip archives through ``/vsizip/``).

    Args:
        path: The file.

    Returns:
        The GDAL path.
    """
    if path.suffix != ".zip":
        return str(path)
    shp = [n for n in gdal.ReadDirRecursive(f"/vsizip/{path}") or [] if n.lower().endswith(".shp")]
    return f"/vsizip/{path}/{shp[0]}" if shp else f"/vsizip/{path}"


def extension(field: pa.Field) -> tuple[str, str]:
    """The Arrow extension name and metadata of a field.

    They come from the field metadata, or from the type when an extension type is registered.

    Args:
        field: The field.

    Returns:
        The name and metadata, empty when the field has none.
    """
    if isinstance(field.type, pa.BaseExtensionType):
        serialize = getattr(field.type, "__arrow_ext_serialize__", None)
        return field.type.extension_name, serialize().decode() if serialize else ""
    meta = field.metadata or {}
    return meta.get(b"ARROW:extension:name", b"").decode(), meta.get(
        b"ARROW:extension:metadata", b""
    ).decode()


def geometry_column(table: pa.Table) -> str:
    """The name of a table's geometry column, found by its extension.

    Args:
        table: The table.

    Returns:
        The column name.
    """
    for f in table.schema:
        name = extension(f)[0]
        if name.startswith("geoarrow.") or name == "ogc.wkb":
            return f.name
    raise ValueError("no geometry column")


def column_crs(table: pa.Table) -> osr.SpatialReference | None:
    """The CRS in a geometry column's GeoArrow metadata.

    Args:
        table: The table.

    Returns:
        The CRS, or None when the metadata has none.
    """
    import json

    meta = json.loads(extension(table.schema.field(geometry_column(table)))[1] or "{}")
    crs = meta.get("crs")
    if crs is None:
        return None
    srs = osr.SpatialReference()
    srs.SetFromUserInput(crs if isinstance(crs, str) else json.dumps(crs))
    return srs


def same_crs(a: osr.SpatialReference | None, b: osr.SpatialReference | None) -> bool:
    """Whether two CRSs are the same (both missing counting as the same).

    Args:
        a: A CRS or None.
        b: A CRS or None.

    Returns:
        True when GDAL finds them the same.
    """
    if a is None or b is None:
        return a is None and b is None
    a, b = a.Clone(), b.Clone()
    for srs in (a, b):
        srs.SetAxisMappingStrategy(osr.OAMS_TRADITIONAL_GIS_ORDER)
    return bool(a.IsSame(b))


def to_geometries(table: pa.Table) -> list:
    """The geometries of a table's geometry column, as Shapely geometries.

    Args:
        table: The table.

    Returns:
        One geometry (or None) per row.
    """
    name = geometry_column(table)
    if table.num_rows == 0:
        # (GeoPandas cannot read empty interleaved GeoArrow arrays.)
        return []
    ext = extension(table.schema.field(name))[0]
    if ext in ("geoarrow.wkb", "ogc.wkb"):
        column = table.column(name)
        if isinstance(column.type, pa.BaseExtensionType):
            column = pa.chunked_array([c.storage for c in column.chunks], column.type.storage_type)
        return list(shapely.from_wkb(column.to_pylist()))
    return list(gpd.GeoDataFrame.from_arrow(table.select([name])).geometry)


def multi(g):
    """A geometry in its multi-part form (as native GeoArrow columns of a multi type hold it).

    Args:
        g: A Shapely geometry or None.

    Returns:
        The multi-part geometry.
    """
    if g is None:
        return None
    t = g.geom_type
    if t == "Polygon":
        return shapely.MultiPolygon([g])
    if t == "LineString":
        return shapely.MultiLineString([g])
    if t == "Point":
        return shapely.MultiPoint([g])
    return g


def normalize(g):
    """A geometry in Shapely's normal form (rings starting at their lowest vertex, parts sorted).

    Args:
        g: A Shapely geometry or None.

    Returns:
        The normalized geometry, or None.
    """
    return None if g is None else shapely.normalize(g)


def wkb(g) -> bytes | None:
    """The canonical ISO WKB of a geometry, empty geometries counting as None.

    Args:
        g: A Shapely geometry or None.

    Returns:
        The WKB, or None.
    """
    if g is None or g.is_empty:
        return None
    return shapely.to_wkb(g, byte_order=1, flavor="iso")


def norm(v):
    """A value normalized for comparison across libraries.

    Args:
        v: A value from a pyarrow column or terrio.

    Returns:
        The value: NaN as None, numbers as floats, dates and times as ISO strings.
    """
    if v is None:
        return None
    if isinstance(v, bool):
        return v
    if isinstance(v, (int, float)):
        return None if isinstance(v, float) and math.isnan(v) else float(v)
    if isinstance(v, dt.datetime):
        return v.replace(tzinfo=None).isoformat(timespec="milliseconds")
    if isinstance(v, dt.date):
        return v.isoformat()
    if isinstance(v, str):
        # terrio keeps date-times as text where they mix time zones.
        return v.replace(" ", "T") if len(v) >= 19 and v[10] in " T" and v[4] == "-" else v
    return v


def gdal_table(path: Path, layer: str, options: list[str] | None = None) -> pa.Table:
    """A layer read through GDAL's Arrow stream.

    Args:
        path: The file.
        layer: The layer name.
        options: Options of ``GetArrowStream``.

    Returns:
        The table.
    """
    plain_geoarrow()
    ds = gdal.OpenEx(gdal_path(path), gdal.OF_VECTOR)
    lyr = ds.GetLayerByName(layer)
    stream = lyr.GetArrowStreamAsPyArrow(options or [])
    schema = pa.schema(list(stream.schema))
    batches = [pa.RecordBatch.from_struct_array(a) for a in stream]
    table = pa.Table.from_batches([b.cast(schema) for b in batches], schema=schema)
    fid = lyr.GetFIDColumn() or "OGC_FID"
    if fid in table.column_names:
        table = table.drop_columns([fid])
    return table


def compare_tables(ref: pa.Table, got: pa.Table, multipart: bool = False) -> dict:
    """Compares two tables of the same layer.

    Args:
        ref: The reference (GDAL's).
        got: terrio's.
        multipart: Compare the reference geometries in their multi-part form.

    Returns:
        Counts: rows, equal geometries, equal attribute values and attribute values compared.
    """
    out = {"rows": (ref.num_rows, got.num_rows)}
    if ref.num_rows != got.num_rows:
        return out
    a = to_geometries(ref)
    b = to_geometries(got)
    if multipart:
        a = [multi(g) for g in a]
    out["geom"] = sum(wkb(x) == wkb(y) for x, y in zip(a, b, strict=False))
    out["geom_norm"] = sum(
        wkb(normalize(x)) == wkb(normalize(y)) for x, y in zip(a, b, strict=False)
    )
    names = [n for n in ref.column_names if n != geometry_column(ref)]
    same = total = 0
    missing = []
    for n in names:
        if n not in got.column_names:
            missing.append(n)
            continue
        for x, y in zip(ref.column(n).to_pylist(), got.column(n).to_pylist(), strict=False):
            total += 1
            same += norm(x) == norm(y)
    out["attrs"] = (same, total)
    if missing:
        out["missing"] = missing
    return out


def compare_layers(
    layer: terrio.Layer, imported: terrio.Layer, multipart: bool = False, crs: bool = True
) -> dict:
    """Compares a layer terrio imported from Arrow with the one terrio reads from the file.

    Args:
        layer: The layer terrio read.
        imported: The layer imported from Arrow.
        multipart: Compare the read geometries in their multi-part form (as native GeoArrow
            columns of a multi-part type hold them).
        crs: Compare the CRSs (not for Arrow data without one, such as GDAL's default stream).

    Returns:
        Counts: rows, equal geometries, equal attribute values, and whether the CRS agree.
    """
    out = {"rows": (len(layer), len(imported))}
    if len(layer) != len(imported):
        return out
    fix = multi if multipart else (lambda g: g)
    a = [wkb(fix(shapely.from_wkb(w))) if w is not None else None for w in layer.to_wkb()]
    b = [wkb(shapely.from_wkb(w)) if w is not None else None for w in imported.to_wkb()]
    out["geom"] = sum(x == y for x, y in zip(a, b, strict=False))
    out["geom_norm"] = sum(
        wkb(normalize(shapely.from_wkb(x) if x else None))
        == wkb(normalize(shapely.from_wkb(y) if y else None))
        for x, y in zip(a, b, strict=False)
    )
    ca, cb = layer.columns(), imported.columns()
    same = total = 0
    for n, values in ca.items():
        if n not in cb:
            continue
        for x, y in zip(values, cb[n], strict=False):
            total += 1
            same += norm(x) == norm(y)
    out["attrs"] = (same, total)
    if crs:
        a_srs, b_srs = osr.SpatialReference(), osr.SpatialReference()
        if layer.crs_wkt and imported.crs_wkt:
            a_srs.SetFromUserInput(layer.crs_wkt)
            b_srs.SetFromUserInput(imported.crs_wkt)
            out["crs"] = bool(a_srs.IsSame(b_srs))
        else:
            out["crs"] = layer.crs_wkt is None and imported.crs_wkt is None
    return out


def status(r: dict) -> str:
    """How a comparison came out.

    Args:
        r: The counts of a comparison.

    Returns:
        ``ok`` when rows, geometries, attributes (and CRS) agree exactly; ``~ok`` when the only
        differences are geometries equal up to vertex order (rings starting at another vertex);
        ``BAD`` otherwise.
    """
    n, m = r["rows"]
    if n != m or r.get("missing") or r.get("crs") is False:
        return "BAD"
    if "attrs" in r and r["attrs"][0] != r["attrs"][1]:
        return "BAD"
    if r.get("geom", n) == n:
        return "ok"
    return "~ok" if r.get("geom_norm") == n else "BAD"


def compare_layer(path: Path, layer: terrio.Layer, label: str, results: list, tmp: str) -> None:
    """Runs every comparison of one layer.

    Args:
        path: The file.
        layer: The layer terrio read from it.
        label: The label of the results.
        results: The results, appended to.
        tmp: A GDAL path for a scratch GeoPackage.
    """
    gds = gdal.OpenEx(gdal_path(path), gdal.OF_VECTOR)
    glyr = gds.GetLayerByName(layer.name)
    gdal_srs = glyr.GetSpatialRef()
    ref = gdal_table(path, layer.name, ["GEOMETRY_METADATA_ENCODING=GEOARROW"])
    t_wkb = to_table(layer.to_arrow())
    results.append((label, "export WKB vs GDAL", compare_tables(ref, t_wkb)))
    crs_ok = same_crs(gdal_srs, column_crs(t_wkb))
    results.append((label, "export CRS vs GDAL", {"rows": (1, 1), "attrs": (int(crs_ok), 1)}))

    single = layer.geometry_type.replace(" Z", "") not in ("Unknown", "GeometryCollection", "None")
    if single:
        for coords in ("separated", "interleaved"):
            t = to_table(layer.to_arrow(geometry_encoding="geoarrow", coordinates=coords))
            ext = extension(t.schema.field("geometry"))[0]
            multipart = ext.startswith("geoarrow.multi")
            results.append(
                (label, f"export {ext} ({coords}) vs GDAL", compare_tables(ref, t, multipart))
            )
            results.append(
                (
                    label,
                    f"import {ext} ({coords})",
                    compare_layers(layer, terrio.from_arrow(t), multipart),
                )
            )

    # GeoPandas reads terrio's table as GDAL's features.
    g_ref = None
    if len(layer):
        geom = geometry_column(ref)
        g_ref = gpd.GeoDataFrame(
            ref.drop_columns([geom]).to_pandas(),
            geometry=to_geometries(ref),
            crs=gdal_srs.ExportToWkt() if gdal_srs else None,
        )
        g_got = gpd.GeoDataFrame.from_arrow(layer.to_arrow())
        ga, gb = np.asarray(g_ref.geometry.array), np.asarray(g_got.geometry.array)
        missing = shapely.is_missing(ga) & shapely.is_missing(gb)
        geom_same = int((shapely.equals_exact(ga, gb, tolerance=0) | missing).sum())
        geom_norm = int(
            (
                shapely.equals_exact(shapely.normalize(ga), shapely.normalize(gb), tolerance=0)
                | missing
            ).sum()
        )
        crs_same = (g_ref.crs is None and g_got.crs is None) or (
            g_ref.crs is not None
            and g_got.crs is not None
            and g_ref.crs.equals(g_got.crs, ignore_axis_order=True)
        )
        attrs = [c for c in g_ref.columns if c != g_ref.geometry.name and c in g_got.columns]

        def values(df, c):
            return df[c].astype(object).where(df[c].notna(), None)

        same = sum(
            norm(x) == norm(y)
            for c in attrs
            for x, y in zip(values(g_ref, c), values(g_got, c), strict=False)
        )
        results.append(
            (
                label,
                "GeoDataFrame.from_arrow vs GDAL",
                {
                    "rows": (len(g_ref), len(g_got)),
                    "geom": geom_same,
                    "geom_norm": geom_norm,
                    "attrs": (same, len(attrs) * len(g_ref)),
                    "crs": crs_same,
                },
            )
        )

    # terrio imports GDAL's and GeoPandas' tables.
    results.append(
        (label, "import GDAL stream (WKB)", compare_layers(layer, terrio.from_arrow(ref)))
    )
    if hasattr(glyr, "__arrow_c_stream__"):
        # GDAL's default stream (ogc.wkb) has no CRS.
        results.append(
            (label, "import ogr.Layer", compare_layers(layer, terrio.from_arrow(glyr), crs=False))
        )
    if g_ref is not None:
        results.append(
            (
                label,
                "import GeoDataFrame.to_arrow(WKB)",
                compare_layers(layer, terrio.from_arrow(g_ref.to_arrow())),
            )
        )
        if single:
            try:
                native = g_ref.to_arrow(geometry_encoding="geoarrow")
            except Exception as e:  # GeoPandas refuses some mixes of types.
                print(f"{label}: GeoPandas to_arrow(geoarrow): {e}", file=sys.stderr)
            else:
                multipart = extension(to_table(native).schema.field(g_ref.geometry.name))[
                    0
                ].startswith("geoarrow.multi")
                results.append(
                    (
                        label,
                        "import GeoDataFrame.to_arrow(geoarrow)",
                        compare_layers(layer, terrio.from_arrow(native), multipart),
                    )
                )

    # GDAL imports terrio's table.
    out = ogr.GetDriverByName("GPKG").CreateDataSource(tmp)
    lyr = out.CreateLayer(layer.name, gdal_srs, ogr.wkbUnknown, ["GEOMETRY_NAME=geometry"])
    lyr.WriteArrow(layer.to_arrow())
    out.Close()
    back = gdal_table(Path(tmp), layer.name, ["GEOMETRY_METADATA_ENCODING=GEOARROW"])
    back = back.rename_columns(
        ["geometry" if n == geometry_column(back) else n for n in back.column_names]
    )
    results.append((label, "GDAL WriteArrow of terrio's table", compare_tables(ref, back)))
    gdal.Unlink(tmp)


def main() -> int:
    """Runs the comparisons.

    Returns:
        The exit status: 1 when something differs.
    """
    p = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    p.add_argument("data", type=Path)
    args = p.parse_args()
    results = []
    tmp = "/vsimem/compare_arrow.gpkg"
    for name, wanted in DATASETS:
        path = args.data / name
        if not path.exists():
            print(f"skip {name}: missing", file=sys.stderr)
            continue
        ds = (
            terrio.open_vector(str(path), layers=wanted)
            if wanted
            else terrio.open_vector(str(path))
        )
        for layer in ds:
            label = f"{name}:{layer.name}"
            try:
                compare_layer(path, layer, label, results, tmp)
            except Exception as e:  # report and go on
                results.append((label, f"error: {e}", {"rows": (0, 1)}))
    width = max(len(a) + len(b) for a, b, _ in results) + 3
    counts = {"ok": 0, "~ok": 0, "BAD": 0}
    for label, what, r in results:
        st = status(r)
        counts[st] += 1
        detail = f"rows {r['rows'][0]}"
        if "geom" in r:
            detail += f", geometries {r['geom']}/{r['rows'][0]}"
            if r["geom"] != r["rows"][0] and "geom_norm" in r:
                detail += f" ({r['geom_norm']} equal up to vertex order)"
        if "attrs" in r:
            detail += f", values {r['attrs'][0]}/{r['attrs'][1]}"
        if "crs" in r:
            detail += f", crs {'same' if r['crs'] else 'DIFFERENT'}"
        if r.get("missing"):
            detail += f", missing columns {r['missing']}"
        print(f"{st:<3} {(label + ' ' + what).ljust(width)} {detail}")
    print(
        f"\n{counts['ok']} of {len(results)} comparisons identical, {counts['~ok']} with "
        f"geometries equal up to vertex order, {counts['BAD']} different"
    )
    return 1 if counts["BAD"] else 0


if __name__ == "__main__":
    sys.exit(main())
