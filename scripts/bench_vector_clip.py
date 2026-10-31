"""Time real-road clipping with a dense circular template and verify against GDAL.

Run with the same data/feature/vertex arguments before and after changing the
implementation. Timings exclude fixture preparation and GDAL comparison reads.
"""
import argparse
import json
from pathlib import Path
import tempfile
import time

from osgeo import ogr
from compare_vector_clip_with_gdal import compare, prepare, run


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('data', type=Path)
    parser.add_argument('--terrio', default='target/release/terrio')
    parser.add_argument('--kind', choices=['roads','counties'], default='roads')
    parser.add_argument('--features', type=int, default=5000)
    parser.add_argument('--vertices', type=int, default=10000)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix='terrio-dense-clip-') as temp:
        root = Path(temp)
        source = root / 'source.gpkg'
        filename, epsg = ('las_vegas_roads.geojson', 32611) if args.kind == 'roads' else ('TN_Counties.geojson', 5070)
        prepare(args.data / filename, source, epsg, limit=args.features)
        ds = ogr.Open(str(source))
        layer = ds.GetLayer(0)
        count = layer.GetFeatureCount()
        x0, x1, y0, y1 = layer.GetExtent()
        center = ogr.Geometry(ogr.wkbPoint)
        center.AddPoint_2D((x0+x1)/2, (y0+y1)/2)
        circle = center.Buffer(min(x1-x0, y1-y0)*.45, max(1, args.vertices//4))
        mask = root / 'dense.gpkg'
        output = ogr.GetDriverByName('GPKG').CreateDataSource(str(mask))
        region = output.CreateLayer('mask', srs=layer.GetSpatialRef(), geom_type=ogr.wkbPolygon)
        feature = ogr.Feature(region.GetLayerDefn())
        feature.SetGeometry(circle)
        region.CreateFeature(feature)
        output = None
        reference, actual = root/'reference.gpkg', root/'actual.gpkg'
        start = time.perf_counter()
        run(['gdal','vector','clip',source,reference,'--like',mask])
        reference_time = time.perf_counter()-start
        start = time.perf_counter()
        run([args.terrio,'vector-clip',source,actual,'--like',mask])
        terrio_time = time.perf_counter()-start
        status, detail = compare(reference,actual)
        print(json.dumps(dict(source=args.kind,source_features=count, template_vertices=circle.GetGeometryRef(0).GetPointCount(),
                              terrio_seconds=terrio_time, gdal_seconds=reference_time,
                              status=status, detail=detail),indent=2))


if __name__ == '__main__':
    main()
