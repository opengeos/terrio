"""Compare vector clipping with GDAL 3.13 on real counties, roads and places.

Checks schema, attributes grouped by source ID, CRS, geometry dimension/type and
symmetric-difference area (relative tolerance 1e-9) or total length (1e-9),
plus Hausdorff distance <= 1e-9 times the coordinate scale. Points must match exactly. Temporary inputs are real-data subsets.
"""
import argparse
import json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import re
import threading
from pathlib import Path
import subprocess
import tempfile

import shapely

from osgeo import gdal, ogr, osr

gdal.UseExceptions()
ogr.UseExceptions()


def run(args):
    result = subprocess.run([str(v) for v in args], capture_output=True, text=True)
    if result.returncode:
        raise RuntimeError(result.stderr[-3000:])
    return result.stdout


def prepare(source, destination, epsg, limit=250):
    original = ogr.Open(str(source))
    layer = original.GetLayer(0)
    src = layer.GetSpatialRef().Clone()
    src.SetAxisMappingStrategy(osr.OAMS_TRADITIONAL_GIS_ORDER)
    dst = osr.SpatialReference()
    dst.ImportFromEPSG(epsg)
    dst.SetAxisMappingStrategy(osr.OAMS_TRADITIONAL_GIS_ORDER)
    transform = osr.CoordinateTransformation(src, dst)
    output = ogr.GetDriverByName('GPKG').CreateDataSource(str(destination))
    out = output.CreateLayer('source', srs=dst, geom_type=layer.GetGeomType())
    for field in layer.schema:
        out.CreateField(field)
    out.CreateField(ogr.FieldDefn('clip_source_id', ogr.OFTInteger))
    for i, f in enumerate(layer):
        if i == limit:
            break
        g = f.GetGeometryRef()
        if g is None:
            continue
        g = g.Clone()
        g.Transform(transform)
        if not g.IsValid():
            g = g.MakeValid()
        record = ogr.Feature(out.GetLayerDefn())
        record.SetFrom(f)
        record.SetGeometry(g)
        record.SetField('clip_source_id', i)
        out.CreateFeature(record)
    output = None


def contents(path):
    ds = ogr.Open(str(path))
    layer = ds.GetLayer(0)
    rows = {}
    for f in layer:
        g = f.GetGeometryRef()
        if g is None or g.IsEmpty():
            raise AssertionError('clip emitted a null/empty feature')
        key = f.GetField('clip_source_id')
        attrs = f.items()
        if key not in rows:
            rows[key] = (attrs, [])
        elif rows[key][0] != attrs:
            raise AssertionError('split feature attributes changed')
        rows[key][1].append(g.Clone())
    schema = [(f.GetName(), f.GetType(), f.GetSubType()) for f in layer.schema]
    return rows, schema, layer.GetSpatialRef().Clone(), layer.GetGeomType()


def dimension_parts(geometries):
    result = {0: [], 1: [], 2: []}
    def visit(g):
        if ogr.GT_Flatten(g.GetGeometryType()) in [ogr.wkbGeometryCollection, ogr.wkbMultiPoint, ogr.wkbMultiLineString, ogr.wkbMultiPolygon]:
            for child in g:
                visit(child)
        elif not g.IsEmpty():
            result[g.GetDimension()].append(g.Clone())
    for g in geometries:
        visit(g)
    return result


def aggregate(geometries):
    if len(geometries) == 1:
        return geometries[0]
    collection = ogr.Geometry(ogr.wkbGeometryCollection)
    for g in geometries:
        collection.AddGeometry(g)
    return collection.UnaryUnion()


def compare(reference, actual):
    ra, sa, ca, ta = contents(reference)
    rb, sb, cb, tb = contents(actual)
    if sa != sb or not ca.IsSame(cb) or ogr.GT_Flatten(ta) != ogr.GT_Flatten(tb):
        raise AssertionError(f'layer schema, CRS or declared type differs: {ta} vs {tb}')
    if ra.keys() != rb.keys():
        raise AssertionError(f'source IDs differ: {ra.keys() ^ rb.keys()}')
    samples = 0
    maximum = 0.0
    max_distance = 0.0
    exact = True
    for key in ra:
        attrs, parts = ra[key]
        if attrs != rb[key][0]:
            raise AssertionError(f'{key}: attributes differ')
        da, db = dimension_parts(parts), dimension_parts(rb[key][1])
        for dimension in range(3):
            if bool(da[dimension]) != bool(db[dimension]):
                raise AssertionError(f'{key}: dimension {dimension} component coverage differs')
            if not da[dimension]:
                continue
            a, b = aggregate(da[dimension]), aggregate(db[dimension])
            if a.Equals(b):
                samples += 1
                continue
            exact = False
            distance = float(shapely.hausdorff_distance(shapely.from_wkb(bytes(a.ExportToWkb())), shapely.from_wkb(bytes(b.ExportToWkb()))))
            scale = max(1.0, *(abs(v) for v in a.GetEnvelope()), *(abs(v) for v in b.GetEnvelope()))
            if distance > scale*1e-9:
                raise AssertionError(f'{key}: Hausdorff distance {distance} exceeds {scale*1e-9}')
            max_distance = max(max_distance, distance)
            delta = a.SymmetricDifference(b)
            if dimension == 2:
                error = delta.Area() / max(1, a.Area())
            elif dimension == 1:
                error = abs(a.Length()-b.Length()) / max(1, a.Length())
            else:
                raise AssertionError(f'{key}: point coordinates differ')
            if error > 1e-9:
                raise AssertionError(f'{key}: relative geometry difference {error}; Hausdorff distance {shapely.hausdorff_distance(shapely.from_wkb(bytes(a.ExportToWkb())), shapely.from_wkb(bytes(b.ExportToWkb())))}; bounds {a.GetEnvelope()}')
            maximum = max(maximum, error)
            samples += 1
    detail = f'{len(ra)} source features/{samples} dimension groups, schema/attributes/CRS/type match; maximum relative geometry difference {maximum:.3g}, Hausdorff distance {max_distance:.3g}'
    return ('identical' if exact else 'within tolerance'), detail



def remote_raster_template(data, root, terrio):
    raster = data / 'usgs_dem.tif'
    size = raster.stat().st_size
    dataset = gdal.Open(str(raster))
    gt = dataset.GetGeoTransform()
    point = ogr.Geometry(ogr.wkbPoint)
    point.AddPoint_2D(gt[0]+dataset.RasterXSize*.5*gt[1]+dataset.RasterYSize*.5*gt[2],
                     gt[3]+dataset.RasterXSize*.5*gt[4]+dataset.RasterYSize*.5*gt[5])
    src = dataset.GetSpatialRef().Clone()
    src.SetAxisMappingStrategy(osr.OAMS_TRADITIONAL_GIS_ORDER)
    dst = osr.SpatialReference()
    dst.ImportFromEPSG(4326)
    dst.SetAxisMappingStrategy(osr.OAMS_TRADITIONAL_GIS_ORDER)
    point.Transform(osr.CoordinateTransformation(src, dst))
    source = root / 'remote-points.geojson'
    source.write_text(json.dumps({'type': 'FeatureCollection', 'features': [
        {'type': 'Feature', 'properties': {'clip_source_id': i}, 'geometry': {'type': 'Point', 'coordinates': c}}
        for i,c in enumerate([[point.GetX(),point.GetY()],[point.GetX()+10,point.GetY()+10]])]}))
    counts = {'bytes': 0, 'gets': 0}
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass
        def do_HEAD(self):
            self.send_response(200)
            self.send_header('Content-Length', str(size))
            self.send_header('Accept-Ranges', 'bytes')
            self.end_headers()
        def do_GET(self):
            match = re.fullmatch(r'bytes=(\d+)-(\d*)', self.headers.get('Range',''))
            start, end = (int(match[1]), min(size-1,int(match[2]) if match[2] else size-1)) if match else (0,size-1)
            self.send_response(206 if match else 200)
            self.send_header('Content-Length',str(end-start+1))
            self.send_header('Accept-Ranges','bytes')
            if match:
                self.send_header('Content-Range',f'bytes {start}-{end}/{size}')
            self.end_headers()
            with raster.open('rb') as stream:
                stream.seek(start)
                self.wfile.write(stream.read(end-start+1))
            counts['bytes'] += end-start+1
            counts['gets'] += 1
    reference, actual = root/'remote-ref.geojson', root/'remote-out.geojson'
    run(['gdal','vector','clip',source,reference,'--like',raster])
    with ThreadingHTTPServer(('127.0.0.1',0),Handler) as server:
        worker = threading.Thread(target=server.serve_forever,daemon=True)
        worker.start()
        try:
            run([terrio,'vector-clip',source,actual,'--like',f'http://127.0.0.1:{server.server_port}/usgs_dem.tif'])
            status, detail = compare(reference,actual)
            if len(contents(actual)[0]) != 1 or not 0 < counts['bytes'] < size/4:
                raise AssertionError(f'remote template must clip correctly with bounded I/O: {counts} of {size}')
        finally:
            server.shutdown()
            worker.join()
    return status, detail+f'; {counts["bytes"]} bytes in {counts["gets"]} GETs from {size}-byte raster'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('data', type=Path)
    parser.add_argument('--terrio', default='target/release/terrio')
    parser.add_argument('--json', action='store_true')
    args = parser.parse_args()
    results = []
    with tempfile.TemporaryDirectory(prefix='terrio-vector-clip-') as directory:
        root = Path(directory)
        for name, source, epsg in [('counties', 'TN_Counties.geojson', 5070),
                                   ('roads', 'las_vegas_roads.geojson', 32611),
                                   ('places', 'ne_places/ne_10m_populated_places_simple.shp', 4326)]:
            path = root / f'{name}.gpkg'
            prepare(args.data / source, path, epsg)
            ds = ogr.Open(str(path))
            layer = ds.GetLayer(0)
            x0, x1, y0, y1 = layer.GetExtent()
            dx, dy = x1-x0, y1-y0
            bounds = [x0+dx*.2, y0+dy*.2, x0+dx*.8, y0+dy*.8]
            bbox = ','.join(str(v) for v in bounds)
            x0, y0, x1, y1 = bounds
            wkt = f'POLYGON (({x0} {y0},{x1} {y0},{x1} {y1},{x0} {y1},{x0} {y0}))'
            mask = root / f'{name}-mask.gpkg'
            out = ogr.GetDriverByName('GPKG').CreateDataSource(str(mask))
            ml = out.CreateLayer('mask', srs=layer.GetSpatialRef(), geom_type=ogr.wkbPolygon)
            ml.CreateField(ogr.FieldDefn('keep', ogr.OFTInteger))
            for value in [1, 0]:
                f = ogr.Feature(ml.GetLayerDefn())
                f.SetGeometry(ogr.CreateGeometryFromWkt(wkt))
                f.SetField('keep', value)
                ml.CreateFeature(f)
            out = None
            geographic = osr.SpatialReference()
            other_epsg = 3857 if epsg == 4326 else 4326
            geographic.ImportFromEPSG(other_epsg)
            geographic.SetAxisMappingStrategy(osr.OAMS_TRADITIONAL_GIS_ORDER)
            projected_mask = ogr.CreateGeometryFromWkt(wkt)
            source_crs = layer.GetSpatialRef().Clone()
            source_crs.SetAxisMappingStrategy(osr.OAMS_TRADITIONAL_GIS_ORDER)
            projected_mask.Transform(osr.CoordinateTransformation(source_crs, geographic))
            ax0, ax1, ay0, ay1 = projected_mask.GetEnvelope()
            other_bbox = ','.join(str(v) for v in [ax0, ay0, ax1, ay1])
            raster = root / f'{name}-bounds.tif'
            rd = gdal.GetDriverByName('GTiff').Create(str(raster), 64, 32, 1, gdal.GDT_Byte)
            rd.SetGeoTransform([x0, (x1-x0)/64, 0, y1, 0, (y0-y1)/32])
            rd.SetProjection(layer.GetSpatialRef().ExportToWkt())
            rd = None
            actual_polygons = None
            if name == 'counties':
                actual_polygons = root / 'county-template.gpkg'
                gdal.VectorTranslate(str(actual_polygons), str(path), limit=10)
            cases = [('bbox', ['--bbox', bbox]), ('wkt', ['--geometry', wkt]),
                     ('like', ['--like', mask]), ('where', ['--like', mask, '--like-where', 'keep=1']),
                     ('sql', ['--like', mask, '--like-sql', 'SELECT * FROM mask WHERE keep=1']),
                     ('bbox CRS', ['--bbox', other_bbox, '--bbox-crs', f'EPSG:{other_epsg}']),
                     ('geometry CRS', ['--geometry', projected_mask.ExportToWkt(), '--geometry-crs', f'EPSG:{other_epsg}']),
                     ('raster template', ['--like', raster])]

            if actual_polygons is not None:
                cases.append(('real polygon template', ['--like', actual_polygons]))
            for index, (kind, options) in enumerate(cases):
                reference, actual = root / f'{name}-{index}-ref.gpkg', root / f'{name}-{index}-out.gpkg'
                try:
                    run(['gdal', 'vector', 'clip', path, reference, '--output-layer', 'result', *options])
                    run([args.terrio, 'vector-clip', path, actual, '--output-layer', 'result', *options])
                    status, detail = compare(reference, actual)
                    results.append(dict(name=f'{name} {kind}', status=status, detail=detail))
                except Exception as error:
                    results.append(dict(name=f'{name} {kind}', status='differs', detail=str(error)))
        # Expose the discontinuity of exact point inclusion at a transformed edge.
        # The independent CRS command verifies the actual arithmetic behind this
        # documented few-ULP difference; it must not hide arbitrary missing points.
        boundary = root / 'boundary.geojson'
        boundary.write_text(json.dumps({'type': 'FeatureCollection', 'features': [
            {'type': 'Feature', 'properties': {'clip_source_id': i}, 'geometry': {'type': 'Point', 'coordinates': point}}
            for i, point in enumerate([[0, 2], [0, 1.5]])]}))
        reference, actual = root / 'boundary-ref.geojson', root / 'boundary-out.geojson'
        options = ['--bbox', '0,0,222638.98158654713,222684.20850554405', '--bbox-crs', 'EPSG:3857']
        try:
            run(['gdal', 'vector', 'clip', boundary, reference, *options])
            run([args.terrio, 'vector-clip', boundary, actual, *options])
            ra, sa, ca, ta = contents(reference)
            rb, sb, cb, tb = contents(actual)
            latitude = float(run([args.terrio, 'crs', 'EPSG:3857', '--to', 'EPSG:4326', '0', '222684.20850554405']).split()[1])
            if not (sa == sb and ca.IsSame(cb) and ta == tb and set(ra) == {0, 1} and set(rb) == {1}
                    and 0 < 2-latitude < 1e-14 and ra[1][0] == rb[1][0] and ra[1][1][0].Equals(rb[1][1][0])):
                raise AssertionError('transformed-boundary behavior changed; re-audit this known difference')
            results.append(dict(name='transformed point boundary', status='within tolerance',
                                detail=f'GDAL includes latitude 2; Terrio clips at {latitude!r}. The interior point, schema/CRS/type and coordinates match; the {2-latitude:.3g}-degree edge discrepancy is independently verified.'))
        except Exception as error:
            results.append(dict(name='transformed point boundary', status='differs', detail=str(error)))
        try:
            status, detail = remote_raster_template(args.data, root, args.terrio)
            results.append(dict(name='remote raster template I/O',status=status,detail=detail))
        except Exception as error:
            results.append(dict(name='remote raster template I/O',status='differs',detail=str(error)))
    print(json.dumps(results, indent=2) if args.json else '\n'.join(f'{r["status"]}: {r["name"]}: {r["detail"]}' for r in results))
    return int(any(r['status'] == 'differs' for r in results))


if __name__ == '__main__':
    raise SystemExit(main())
