"""Independent h5py oracle for terrio's HECRAS driver (crates/terrio/tests/hecras_corpus.rs).

Reads HEC-RAS geometry/plan HDF files with h5py and NumPy only (not terrio or its HEC-RAS engine)
and writes, per file: the stored Projection and Units System, and per 2D flow area: drawn/stored
cell counts, physical cell count, face count and a digest of every face polyline (start
facepoint, stored perimeter vertices, end facepoint), a digest of the stored perimeter ring, and
digests of the Maximum Water Surface (cells) and Maximum Face Velocity (faces) value and time
rows, of the static cell and face arrays (minimum elevations, Manning's n, face normals and
lengths, face cell indexes), of Water Surface / Face Velocity at one stored output row and of
Water Surface's maximum over all rows (value at the first maximum, and its row); the count and
identity digest of the 1D cross sections, river reaches, storage areas, structures and 2D
boundary-condition lines; and per-row digests of the 1D cross-section summaries and of the
cross-section results at one stored row (unsteady) or profile (steady), in geometry
attribute-row order; and the stored attributes terrio reads without h5py: every column of every
family attribute table (field type and values), the 2D flow area table, face points, pipe
conduits and nodes, the cross-section station-elevation, Manning's n and ineffective-area tables,
and the root attributes.

Derivation: this oracle adapts the h5py reading of the ras-commander Python library
(https://github.com/gpt-cmdr/ras-commander, MIT, Copyright (c) 2024 William M. Katzenmeyer; see
NOTICE). The face polylines follow ``HdfMesh.get_mesh_cell_faces`` (start face point, the
stored ``Faces Perimeter Values`` rows of ``Faces Perimeter Info``, end face point); the rest
reads the stored datasets directly, by the names HEC-RAS writes. It shares no code with terrio or
its engine.

Usage (``scripts/compare_hecras_with_h5py.py`` runs it):

    python -I scripts/hecras_oracle.py <corpus-dir> <out.json> <relative file>...

<corpus-dir> holds HEC-RAS files, such as the fixtures of the public ras-hdf-test-corpus.
Models are never committed.
"""
import hashlib
import json
import struct
import sys
from pathlib import Path

import h5py
import numpy as np

BASE = "Results/Unsteady/Output/Output Blocks/Base Output"
SUMMARY = f"{BASE}/Summary Output/2D Flow Areas"
SERIES = f"{BASE}/Unsteady Time Series"
STEADY = "Results/Steady/Output/Output Blocks/Base Output/Steady Profiles"
XS_SUMMARIES = ["Maximum Water Surface", "Maximum Flow", "Maximum Channel Velocity"]
XS_RESULTS = ["Water Surface", "Flow"]


def text(v):
    if isinstance(v, (bytes, np.bytes_)):
        return bytes(v).decode("utf-8")
    if isinstance(v, np.ndarray) and v.shape == ():
        return text(v[()])
    return str(v)


def digest(values):
    return hashlib.sha256(np.asarray(values, dtype="<f8").tobytes()).hexdigest()


def int_digest(values):
    return hashlib.sha256(np.asarray(values, dtype="<i8").tobytes()).hexdigest()


def columns_digest(ds, rows):
    """Per stored column of an (elements,) or (elements, k) array, the digest over `rows`."""
    v = ds[()]
    v = v.reshape(len(v), -1)
    pick = int_digest if v.dtype.kind in "iu" else digest
    return [pick(v[rows, c]) for c in range(v.shape[1])]


def time_row(f):
    """The stored output row the oracle reads time-step values at: the middle row."""
    t = f.get(f"{SERIES}/Time")
    return None if t is None else len(t) // 2


def norm(v):
    return " ".join(text(v).split())


def structure_rows(f, row):
    """`Structure Variables` at `row` per Geometry/Structures/Attributes row, linked by name:
    a lateral, inline, bridge or culvert group is named `<River> <Reach> <RS>` and matches the
    row of that `Type`; an SA/2D connection group is named after its `Connection` (or
    `<US SA/2D> <Connection>`). Each group must name exactly one row."""
    table = f.get("Geometry/Structures/Attributes")
    if table is None or row is None:
        return {}
    rows = table[()]
    out = {}
    kinds = {"Lateral Structures": "Lateral", "Inline Structures": "Inline", "Bridges": "Bridge", "Culverts": "Culvert"}
    for family in [*kinds, "SA 2D Area Conn"]:
        g = f.get(f"{SERIES}/{family}")
        if g is None:
            continue
        for name, group in g.items():
            if not isinstance(group, h5py.Group) or "Structure Variables" not in group:
                continue
            if family in kinds:
                hits = [i for i, r in enumerate(rows) if norm(r["Type"]) == kinds[family] and f"{norm(r['River'])} {norm(r['Reach'])} {norm(r['RS'])}" == norm(name)]
            else:
                # Named after the Connection, or `<US SA/2D> <Connection>` (HEC-RAS 6.6).
                hits = [i for i, r in enumerate(rows) if norm(name) in (norm(r["Connection"]), f"{norm(r['US SA/2D'])} {norm(r['Connection'])}")]
            assert len(hits) == 1, (family, name, hits)
            ds = group["Structure Variables"]
            labels = [text(l[0]) for l in ds.attrs["Variable_Unit"]]
            out[str(hits[0])] = {"labels": labels, "values_sha256": digest(ds[row])}
    return out


def bc_rows(f, row):
    """Each 2D boundary-condition line's own series (named after the line) at `row`, by
    attribute row; columns from the stored `Columns` labels."""
    table = f.get("Geometry/Boundary Condition Lines/Attributes")
    if table is None or row is None:
        return {}
    out = {}
    for i, r in enumerate(table[()]):
        ds = f.get(f"{SERIES}/2D Flow Areas/{norm(r['SA-2D'])}/Boundary Conditions/{norm(r['Name'])}")
        if ds is not None and "Columns" in ds.attrs:
            out[str(i)] = {"labels": [text(c) for c in ds.attrs["Columns"]], "values_sha256": digest(ds[row])}
    return out


def reference_rows(f, row, family, area_column):
    """Per variable stored with one value per Time row and Name column, the geometry
    Attributes row of each column and the digest of the values at `row` in that column order.
    Columns are linked by the exact `Name|<area>`; lines also by Internal Faces against the Name
    axis's `<Name>|Faces`, points by the series' `Cells` against the geometry `Cell Index`."""
    group = f.get(f"{SERIES}/{family}")
    geometry = f.get(f"Geometry/{family}/Attributes")
    if group is None or geometry is None or row is None or "Name" not in group:
        return {}
    attrs = geometry[()]
    keys = [f"{text(a['Name']).rstrip()}|{text(a[area_column]).rstrip()}" for a in attrs]
    name_ds = group["Name"]
    rows = []
    for entry in (text(n).rstrip() for n in name_ds[()]):
        hits = [i for i, k in enumerate(keys) if k == entry]
        assert len(hits) == 1, (family, entry, hits)
        rows.append(hits[0])
    if family == "Reference Lines":
        internal = f[f"Geometry/{family}/Internal Faces"][()]
        for r in rows:
            faces = internal["Face Index"][internal["Reference Line ID"] == r]
            assert list(faces) == list(name_ds.attrs[f"{text(attrs[r]['Name']).rstrip()}|Faces"]), (family, r)
    time_rows = len(f[f"{SERIES}/Time"])
    out = {}
    for name, d in group.items():
        if name == "Name" or not isinstance(d, h5py.Dataset) or d.shape != (time_rows, len(rows)):
            continue
        if family == "Reference Points":
            assert list(d.attrs["Cells"]) == [int(attrs[r]["Cell Index"]) for r in rows], (family, name)
        out[name] = {"rows": rows, "values_sha256": digest(d[row])}
    return out


def lines_digest(lines):
    h = hashlib.sha256()
    for coords in lines:
        h.update(struct.pack("<Q", len(coords)))
        for x, y in coords:
            h.update(struct.pack("<dd", float(x), float(y)))
    return h.hexdigest()


def strings_digest(rows):
    return hashlib.sha256("\n".join("|".join(r) for r in rows).encode()).hexdigest()


def table_rows(f, path, fields):
    if path not in f:
        return None
    rows = f[path][()]
    names = rows.dtype.names or ()
    return [[text(r[k]).rstrip() if k in names else "" for k in fields] for r in rows]


def summary(ds, columns):
    v = ds[()]
    return {
        "location": text(ds.attrs["Location"]) if "Location" in ds.attrs else None,
        "value_sha256": digest(v[0][columns]),
        "time_sha256": digest(v[1][columns]) if v.shape[0] == 2 else None,
    }


# Stored attribute tables of the family layers, by terrio layer name, with the geometry dataset
# a feature needs (a table without its geometry, such as the attribute row of a structure
# without a stored centerline, gives no layer). Every column of each is compared: its field type
# and every value, in attribute-row (`source_row`) order.
ATTRIBUTE_TABLES = [
    ("cross_sections", "Geometry/Cross Sections/Attributes", "Polyline Info"),
    ("river_centerlines", "Geometry/River Centerlines/Attributes", "Polyline Info"),
    ("river_bank_lines", "Geometry/River Bank Lines/Attributes", "Polyline Info"),
    ("river_edge_lines", "Geometry/River Edge Lines/Attributes", "Polyline Info"),
    ("river_flow_paths", "Geometry/River Flow Paths/Attributes", "Flow Path Lines Info"),
    ("storage_areas", "Geometry/Storage Areas/Attributes", "Polygon Info"),
    ("structures", "Geometry/Structures/Attributes", "Centerline Info"),
    ("bc_lines", "Geometry/Boundary Condition Lines/Attributes", "Polyline Info"),
    ("breaklines", "Geometry/2D Flow Area Break Lines/Attributes", "Polyline Info"),
    ("refinement_regions", "Geometry/2D Flow Area Refinement Regions/Attributes", "Polygon Info"),
    ("reference_lines", "Geometry/Reference Lines/Attributes", "Polyline Info"),
    ("reference_points", "Geometry/Reference Points/Attributes", "Points"),
    ("river_junctions", "Geometry/Junctions/Attributes", "Points"),
    ("pump_stations", "Geometry/Pump Stations/Attributes", "Points"),
    ("pipe_conduits", "Geometry/Pipe Conduits/Attributes", "Polyline Info"),
    ("pipe_nodes", "Geometry/Pipe Nodes/Attributes", "Points"),
]
# Per-section tables of `Geometry/Cross Sections`: (key, Info, Values).
XS_TABLES = [
    ("station_elevation", "Station Elevation Info", "Station Elevation Values"),
    ("mannings_n", "Manning's n Info", "Manning's n Values"),
    ("ineffective_areas", "Ineffective Info", "Ineffective Blocks"),
]


def kind_of(dtype):
    """The terrio field type a stored member maps to: text, int or float (other kinds: bytes)."""
    return {"S": "text", "U": "text", "O": "text", "i": "int", "u": "int", "b": "int", "f": "float"}.get(dtype.kind, "bytes")


def values_digest(values, kind):
    """The digest of one column: text joined by newlines (decoded as h5py gives the bytes, never
    stripped), integers as little-endian i64, floats as little-endian f64 (f32 widened exactly)."""
    if kind == "text":
        return hashlib.sha256("\n".join(text(v) for v in values).encode()).hexdigest()
    if kind == "int":
        return int_digest(np.asarray(values).astype("<i8"))
    if kind == "float":
        return digest(np.asarray(values).astype("<f8"))
    return hashlib.sha256(np.ascontiguousarray(values).tobytes()).hexdigest()


def canonical_nan(values):
    """Floats with every NaN as NumPy's NaN: terrio carries the per-section tables as JSON, which
    holds a NaN as null, so the stored NaN bit pattern is not compared there."""
    v = np.asarray(values)
    return np.where(np.isnan(v), np.nan, v.astype("<f8")) if v.dtype.kind == "f" else v


def compound_columns(rows, nan=False):
    out = []
    for k in rows.dtype.names:
        kind = kind_of(rows.dtype[k])
        v = canonical_nan(rows[k]) if nan else rows[k]
        out.append({"name": k, "kind": kind, "sha256": values_digest(v, kind)})
    return out


def packed_lines(f, group, prefix):
    """Every stored part of every feature of a packed Info/Parts/Points family, in stored order.
    Part offsets are local to the feature (HEC-RAS 6+ layout)."""
    info = f[f"{group}/{prefix} Info"][()]
    parts = f[f"{group}/{prefix} Parts"][()]
    points = f[f"{group}/{prefix} Points"][()]
    lines = []
    for pt_start, _pt_count, part_start, part_count in info:
        for offset, count in parts[part_start:part_start + part_count]:
            lines.append([tuple(p[:2]) for p in points[pt_start + offset:pt_start + offset + count]])
    return lines


def root_attribute(v):
    """A root attribute as the oracle compares it: ("text", str) or ("number", [floats])."""
    if isinstance(v, (bytes, np.bytes_, str)):
        return ["text", text(v)]
    a = np.asarray(v)
    if a.dtype.kind in "SUO":
        return ["text", ", ".join(text(x) for x in a.ravel())]
    return ["number", [float(x) for x in a.ravel()]]


def stored(f, mesh_names):
    """The stored attributes terrio reads without h5py, read here with h5py."""
    out = {"file_attributes": [[k, *root_attribute(f.attrs[k])] for k in f.attrs], "tables": {}}
    for layer, path, geometry in ATTRIBUTE_TABLES:
        if path in f and path.replace("Attributes", geometry) in f:
            rows = f[path][()]
            out["tables"][layer] = {"path": path, "rows": int(len(rows)), "columns": compound_columns(rows)}
    if "Geometry/Pipe Conduits/Polyline Info" in f:
        out["pipe_conduits_lines_sha256"] = lines_digest(packed_lines(f, "Geometry/Pipe Conduits", "Polyline"))
    if "Geometry/Pipe Nodes/Points" in f:
        out["pipe_nodes_points_sha256"] = digest(f["Geometry/Pipe Nodes/Points"][()])
    areas = f.get("Geometry/2D Flow Areas")
    if areas is not None and "Attributes" in areas:
        rows = areas["Attributes"][()]
        by_name = {text(r["Name"]): r for r in rows}
        out["flow_areas"] = {
            name: [[k, kind_of(rows.dtype[k]), values_digest(np.asarray([by_name[name][k]]), kind_of(rows.dtype[k]))] for k in rows.dtype.names]
            for name in mesh_names if name in by_name
        }
    face_points = {}
    for name in mesh_names:
        g = areas[name]
        fp = {"count": int(len(g["FacePoints Coordinate"])), "xy_sha256": digest(g["FacePoints Coordinate"][()])}
        fp["is_perimeter_sha256"] = int_digest(g["FacePoints Is Perimeter"][()]) if "FacePoints Is Perimeter" in g else None
        fp["elevation_sha256"] = digest(g["FacePoints Elevation"][()]) if "FacePoints Elevation" in g else None
        face_points[name] = fp
    out["face_points"] = face_points
    xs = f.get("Geometry/Cross Sections")
    xs_tables = {}
    for key, info_name, values_name in XS_TABLES:
        if xs is None or info_name not in xs or values_name not in xs:
            continue
        info = xs[info_name][()]
        values = xs[values_name][()]
        picked = np.concatenate([values[s:s + c] for s, c in info[:, :2]]) if len(info) else values[:0]
        entry = {"sections": int(len(info)), "counts_sha256": int_digest(info[:, 1])}
        if values.dtype.names:
            entry["columns"] = compound_columns(picked, nan=True)
        else:
            entry["values_sha256"] = digest(canonical_nan(picked.reshape(-1)))
        xs_tables[key] = entry
    out["xs_tables"] = xs_tables
    return out


def main(root, out, files):
    report = {}
    for rel in files:
        path = Path(root) / rel
        with h5py.File(path, "r") as f:
            proj = f.attrs.get("Projection")
            entry = {
                "sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
                "projection": None if proj is None else text(proj),
                "units_system": text(f.attrs["Units System"]) if "Units System" in f.attrs else None,
                "meshes": {},
            }
            areas = f.get("Geometry/2D Flow Areas")
            names = [k for k, v in areas.items() if isinstance(v, h5py.Group) and "Cells Face and Orientation Info" in v] if areas else []
            physical = {}
            if areas is not None and "Attributes" in areas:
                attrs = areas["Attributes"][()]
                if "Cell Count" in (attrs.dtype.names or ()):
                    physical = {text(a["Name"]).rstrip(): int(a["Cell Count"]) for a in attrs}
            for name in names:
                g = areas[name]
                faces_per_cell = g["Cells Face and Orientation Info"][:, 1]
                drawn = np.flatnonzero(faces_per_cell >= 3)
                fp = g["FacePoints Coordinate"][()]
                ends = g["Faces FacePoint Indexes"][()]
                info = g["Faces Perimeter Info"][()]
                values = g["Faces Perimeter Values"][()]
                lines = []
                for (a, b), (start, count) in zip(ends, info):
                    coords = [fp[a]] + list(values[start:start + count]) + [fp[b]]
                    lines.append([(c[0], c[1]) for c in coords])
                mesh = {
                    "stored_cells": int(len(faces_per_cell)),
                    "drawn": int(len(drawn)),
                    "drawn_cell_ids": [int(drawn[0]), int(drawn[-1])],
                    "physical_cells": physical.get(name),
                    "faces": len(lines),
                    "faces_sha256": lines_digest(lines),
                    "perimeter_sha256": None,
                }
                if "Perimeter" in g:
                    ring = [tuple(r[:2]) for r in g["Perimeter"][()]]
                    if ring[0] != ring[-1]:
                        ring.append(ring[0])
                    mesh["perimeter_sha256"] = lines_digest([ring])
                s = f.get(f"{SUMMARY}/{name}")
                if s is not None and "Maximum Water Surface" in s:
                    mesh["maximum_water_surface"] = summary(s["Maximum Water Surface"], drawn)
                if s is not None and "Maximum Face Velocity" in s:
                    mesh["maximum_face_velocity"] = summary(s["Maximum Face Velocity"], np.arange(len(lines)))
                faces = np.arange(len(lines))
                static = {}
                for key, dataset, rows in [
                    ("cells_minimum_elevation", "Cells Minimum Elevation", drawn),
                    ("cells_mannings_n", "Cells Center Manning's n", drawn),
                    ("faces_minimum_elevation", "Faces Minimum Elevation", faces),
                    ("faces_normal_and_length", "Faces NormalUnitVector and Length", faces),
                    ("faces_cells", "Faces Cell Indexes", faces),
                ]:
                    if dataset in g:
                        static[key] = columns_digest(g[dataset], rows)
                mesh["static"] = static
                row = time_row(f)
                ts = f.get(f"{SERIES}/2D Flow Areas/{name}")
                if row is not None and ts is not None:
                    step = {"row": row}
                    if "Water Surface" in ts:
                        wse = ts["Water Surface"][()]
                        step["water_surface"] = digest(wse[row][drawn])
                        at = np.argmax(wse, axis=0)
                        mesh["water_surface_max"] = {
                            "value_sha256": digest(wse[at, np.arange(wse.shape[1])][drawn]),
                            "row_sha256": int_digest(at[drawn]),
                        }
                    if "Face Velocity" in ts:
                        step["face_velocity"] = digest(ts["Face Velocity"][row])
                    # Depth only where HEC-RAS stored it (5.0 plans store a cell Depth series).
                    if "Depth" in ts:
                        depth = ts["Depth"][()]
                        step["depth"] = digest(depth[row][drawn])
                        at = np.argmax(depth, axis=0)
                        mesh["depth_max"] = {
                            "value_sha256": digest(depth[at, np.arange(depth.shape[1])][drawn]),
                            "row_sha256": int_digest(at[drawn]),
                        }
                    mesh["time_step"] = step
                entry["meshes"][name] = mesh
            families = {}
            for layer, table, fields in [
                ("cross_sections", "Geometry/Cross Sections/Attributes", ["River", "Reach", "RS"]),
                ("river_centerlines", "Geometry/River Centerlines/Attributes", ["River Name", "Reach Name"]),
                ("storage_areas", "Geometry/Storage Areas/Attributes", ["Name"]),
                ("structures", "Geometry/Structures/Attributes", ["Type", "River", "Reach", "RS", "Connection"]),
                ("bc_lines", "Geometry/Boundary Condition Lines/Attributes", ["Name", "SA-2D", "Type"]),
                ("river_junctions", "Geometry/Junctions/Attributes", ["Name"]),
                ("pump_stations", "Geometry/Pump Stations/Attributes", ["Name", "Inlet SA/2D", "Outlet SA/2D"]),
            ]:
                rows = table_rows(f, table, fields)
                if rows is not None:
                    families[layer] = {"fields": fields, "count": len(rows), "sha256": strings_digest(rows)}
                    points = table.replace("/Attributes", "/Points")
                    if layer in ("river_junctions", "pump_stations") and points in f:
                        families[layer]["points_sha256"] = digest(f[points][()])
            entry["families"] = families
            one_d = {}
            xs_rows = f["Geometry/Cross Sections/Attributes"].shape[0] if "Geometry/Cross Sections/Attributes" in f else None
            xs = f.get(f"{BASE}/Summary Output/Cross Sections")
            if xs is not None and xs_rows:
                for var in XS_SUMMARIES:
                    if var in xs and xs[var].shape[-1] == xs_rows:
                        v = xs[var][()].reshape(-1, xs_rows)
                        one_d[var] = [digest(r) for r in v]
            row = time_row(f)
            results = f.get(f"{SERIES}/Cross Sections")
            if results is not None and row is not None and xs_rows:
                # The engine labels the row with its date stamp when the plan stores one.
                stamps = f.get(f"{SERIES}/Time Date Stamp")
                stamp = None if stamps is None else text(stamps[row])
                one_d["unsteady"] = {"row": row, "label": stamp, "values": {v: digest(results[v][row]) for v in XS_RESULTS if v in results}}
            steady = f.get(f"{STEADY}/Cross Sections")
            if steady is not None and xs_rows:
                names = [text(n) for n in f[f"{STEADY}/Profile Names"][()]]
                # The second profile, or the only one of a single-profile plan.
                profile = 1 if len(names) > 1 else 0
                one_d["steady"] = {
                    "row": profile,
                    "label": names[profile],
                    "values": {v: digest(steady[v][profile]) for v in XS_RESULTS if v in steady},
                }
            if row is not None and "Time Date Stamp" in f.get(SERIES, {}):
                entry["time_step"] = {"row": row, "date_stamp": text(f[f"{SERIES}/Time Date Stamp"][row]), "days": float(f[f"{SERIES}/Time"][row])}
            entry["one_d"] = one_d
            entry["structures"] = structure_rows(f, row)
            entry["bc_lines"] = bc_rows(f, row)
            entry["reference_lines"] = reference_rows(f, row, "Reference Lines", "SA-2D")
            entry["reference_points"] = reference_rows(f, row, "Reference Points", "SA/2D")
            entry["stored"] = stored(f, list(entry["meshes"]))
        report[rel] = entry
    Path(out).write_text(json.dumps(report, indent=2))


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2], sys.argv[3:])
