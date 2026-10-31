"""Generates `crates/terrio/data/projjson_db.tsv.zst`: what PROJ's database adds to the PROJJSON of a CRS
built from an EPSG code (`CRS.from_epsg`, which GDAL resolves through PROJ's database) beyond its WKT:
the conversion's name, the coordinate system's axes, the usage (scope, area and bounding box), and
the members of datum ensembles.

    python scripts/gen_projjson_table.py <proj.db> <out.tsv.zst>

Use the `proj.db` of the GDAL terrio is compared with (rasterio's wheel bundles it, in
`rasterio/proj_data/proj.db`), so the text is that GDAL's. Lines:

- `C<TAB>code<TAB>conversion name<TAB>axes<TAB>units`, for the EPSG geographic (2D and 3D) and
  projected CRSs; `axes` is `name|abbreviation|orientation|unit code` items separated by `;`;
  `units` is 1 when the conversion's parameters are in degrees, unity and the axes' unit (as
  GDAL's WKT 1 gives them), else 0.
- `D<TAB>datum code<TAB>anchor epoch<TAB>frame reference epoch`, for datums with either.
- `U<TAB>code<TAB>scope<TAB>area<TAB>south<TAB>west<TAB>north<TAB>east`, the CRS's usages in
  the database's order (PROJ writes one as `scope`, `area` and `bbox`, several as `usages`).
- `E<TAB>datum code<TAB>ensemble name<TAB>accuracy<TAB>members`, `members` being
  `code=name` items separated by `;`.
- `L<TAB>name<TAB>semi-major axis<TAB>inverse flattening<TAB>semi-minor axis`, the ellipsoids in
  metres, one of the last two empty (PROJ writes an ellipsoid as the database defines it).
"""

import sqlite3
import subprocess
import sys


def main():
    db, out = sys.argv[1], sys.argv[2]
    c = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    version = dict(c.execute("select key, value from metadata"))
    lines = [f"# PROJ database EPSG {version.get('EPSG.VERSION')} ({version.get('EPSG.DATE')}). scripts/gen_projjson_table.py"]
    axes = {}
    for cs, name, abbrev, orientation, uom, _ in c.execute(
        "select coordinate_system_code, name, abbrev, orientation, uom_code, coordinate_system_order from axis "
        "where coordinate_system_auth_name = 'EPSG' order by coordinate_system_code, coordinate_system_order"
    ):
        axes.setdefault(cs, []).append(f"{name}|{abbrev}|{orientation}|{uom}")
    extents = {
        code: (area, s, w, n, e)
        for code, area, s, w, n, e in c.execute(
            "select code, description, south_lat, west_lon, north_lat, east_lon from extent where auth_name = 'EPSG'"
        )
    }
    usages = {}
    for table, code, scope, extent in c.execute(
        "select u.object_table_name, u.object_code, sc.scope, u.extent_code from usage u join scope sc on "
        "sc.auth_name = u.scope_auth_name and sc.code = u.scope_code where u.object_auth_name = 'EPSG' and "
        "u.extent_auth_name = 'EPSG' and u.object_table_name in ('geodetic_crs', 'projected_crs') order by u.rowid"
    ):
        # PROJ gives "World (by country)" (EPSG:2830) as "World" (EPSG:1262): rasterio 1.5.0's
        # `CRS.from_epsg(4326).to_dict(projjson=True)["area"]` is "World.".
        usages.setdefault((table, code), []).append((scope, *extents[1262 if int(extent) == 2830 else int(extent)]))
    rows = []
    for code, cs in c.execute(
        "select code, coordinate_system_code from geodetic_crs where auth_name = 'EPSG' and deprecated = 0 "
        "and type in ('geographic 2D', 'geographic 3D')"
    ):
        rows.append(("geodetic_crs", code, "", cs))
    uoms = ", ".join(f"t.param{i}_uom_code" for i in range(1, 8))
    units_ok = {}
    for code, cs, conv, *params in c.execute(
        f"select p.code, p.coordinate_system_code, t.name, {uoms} from projected_crs p join conversion_table t on "
        "t.auth_name = p.conversion_auth_name and t.code = p.conversion_code where p.auth_name = 'EPSG' and p.deprecated = 0"
    ):
        rows.append(("projected_crs", code, conv, cs))
        axis_uoms = {a.rsplit("|", 1)[1] for a in axes.get(cs, [])}
        # Degrees (PROJ writes sexagesimal DMS, 9110, in degrees), unity, and lengths in the axes'
        # unit: the parameters of GDAL's WKT 1.
        units_ok[code] = all(u is None or str(u) in {"9102", "9110", "9122", "9201"} | axis_uoms for u in params)
    clean = lambda fields: not any("\t" in str(f) or "\n" in str(f) for f in fields)  # noqa: E731
    for table, code, conv, cs in sorted(rows, key=lambda r: int(r[1])):
        u = usages.get((table, code), [])
        if cs not in axes or not clean([conv, *axes[cs]]) or not all(clean(x) for x in u):
            continue
        lines.append(f"C\t{code}\t{conv}\t{';'.join(axes[cs])}\t{int(units_ok.get(code, True))}")
        lines += ["U\t" + "\t".join(str(f) for f in (code, *x)) for x in u]
    for code, anchor, frame in c.execute(
        "select code, anchor_epoch, frame_reference_epoch from geodetic_datum where auth_name = 'EPSG' and "
        "(anchor_epoch is not null or frame_reference_epoch is not null)"
    ):
        lines.append(f"D\t{code}\t{'' if anchor is None else anchor}\t{'' if frame is None else frame}")
    for name, a, rf, b in c.execute(
        "select name, semi_major_axis, inv_flattening, semi_minor_axis from ellipsoid where auth_name = 'EPSG' "
        "and uom_auth_name = 'EPSG' and uom_code = 9001 and deprecated = 0"
    ):
        lines.append(f"L\t{name}\t{a}\t{'' if rf is None else rf}\t{'' if b is None else b}")
    for code, name, accuracy in c.execute(
        "select code, name, ensemble_accuracy from geodetic_datum where auth_name = 'EPSG' and ensemble_accuracy is not null"
    ):
        members = c.execute(
            "select m.member_code, d.name from geodetic_datum_ensemble_member m join geodetic_datum d on "
            "d.auth_name = m.member_auth_name and d.code = m.member_code where m.ensemble_auth_name = 'EPSG' and "
            "m.ensemble_code = ? order by m.sequence",
            (code,),
        ).fetchall()
        lines.append(f"E\t{code}\t{name}\t{accuracy}\t" + ";".join(f"{m}={n}" for m, n in members))
    text = "\n".join(lines) + "\n"
    # The `zstd` command line tool, as for terrio's other tables.
    open(out, "wb").write(subprocess.run(["zstd", "-19", "-q", "-c"], input=text.encode(), capture_output=True, check=True).stdout)
    print(out, len(lines), "lines,", len(text), "bytes of text")


if __name__ == "__main__":
    main()
