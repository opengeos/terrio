#!/usr/bin/env python3
"""Compare palette generation, mapping, expansion and attachment with GDAL 3.13."""
import argparse
import itertools
import json
import os
import sys
import tempfile

import numpy as np
from osgeo import gdal
from compare_dem_calc_with_gdal import compare, format_row, run
from compare_raster_pixel_with_gdal import raster_extra

gdal.UseExceptions()


def check(reference, actual):
    result = compare(reference, actual, 0)
    result.update(raster_extra(reference, actual))
    a, b = gdal.Open(reference), gdal.Open(actual)
    ta, tb = (ds.GetRasterBand(1).GetColorTable() for ds in (a, b))
    result['same_palette'] = (ta is None and tb is None) or (
        ta is not None and tb is not None and ta.GetCount() == tb.GetCount()
        and all(ta.GetColorEntry(i) == tb.GetColorEntry(i) for i in range(ta.GetCount())))
    result['same_mask'] = all(np.array_equal(
        a.GetRasterBand(i).GetMaskBand().ReadAsArray(),
        b.GetRasterBand(i).GetMaskBand().ReadAsArray()) for i in range(1, a.RasterCount + 1))
    result['same_metadata'] &= result['same_palette'] and result['same_mask']
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('data')
    parser.add_argument('--terrio', default='target/release/terrio')
    parser.add_argument('--json', action='store_true')
    args = parser.parse_args()
    results = []
    with tempfile.TemporaryDirectory(prefix='terrio-palette-') as tmp:
        rgb = os.path.join(tmp, 'rgb.tif')
        ds = gdal.Translate(rgb, os.path.join(args.data, 'landsat.tif'),
                            srcWin=[600, 500, 63, 47], bandList=[1, 2, 3],
                            outputType=gdal.GDT_Byte)
        ds = None
        sources = {'rgb': rgb}
        for kind in ['nodata', 'mask', 'alpha', 'all-invalid', 'one-valid-high']:
            path = os.path.join(tmp, kind + '.tif')
            ds = gdal.Translate(path, rgb, bandList=[1, 2, 3, 1] if kind == 'alpha' else [1, 2, 3])
            if kind == 'nodata':
                for i in range(1, 4):
                    band = ds.GetRasterBand(i)
                    values = band.ReadAsArray()
                    values[:8, :11] = 0
                    band.WriteArray(values)
                    band.SetNoDataValue(0)
            else:
                valid = np.full((47, 63), 255, dtype='uint8')
                valid[:8, :11] = 0
                if kind in ['all-invalid', 'one-valid-high']:
                    valid.fill(0)
                    if kind == 'one-valid-high':
                        valid[0, 0] = 255
                    for band in range(1, 4):
                        ds.GetRasterBand(band).Fill(255)
                if kind == 'alpha':
                    ds.GetRasterBand(4).SetColorInterpretation(gdal.GCI_AlphaBand)
                    ds.GetRasterBand(4).WriteArray(valid)
                else:
                    ds.CreateMaskBand(gdal.GMF_PER_DATASET)
                    ds.GetRasterBand(1).GetMaskBand().WriteArray(valid)
            ds = None
            sources[kind] = path
        path = os.path.join(tmp, 'nodata-mask.tif')
        ds = gdal.Translate(path, sources['nodata'])
        ds.CreateMaskBand(gdal.GMF_PER_DATASET)
        validity = np.full((47, 63), 255, dtype='uint8')
        validity[10:20, 12:22] = 0
        ds.GetRasterBand(1).GetMaskBand().WriteArray(validity)
        ds = None
        sources['nodata-mask'] = path
        cases = []
        for kind, count, bits, dither in itertools.product(sources, [2, 17, 256], [5, 8], [True, False]):
            opts = ['--color-count', str(count), '--bit-depth', str(bits)]
            if not dither:
                opts += ['--no-dither']
            cases.append((f'{kind} colors={count} bits={bits} dither={dither}',
                          sources[kind], opts))
        # Palette-raster and indexed-text mapping, including an explicit nodata index.
        palette = os.path.join(args.data, 'OPERA_L3_DSWx_WTR.tif')
        text = os.path.join(tmp, 'colors.txt')
        with open(text, 'w') as out:
            out.write('0 0 0 0\n1 255 0 0\n2 0 255 0\n3 0 0 255\n')
        for color_map, bits, dither, nodata in itertools.product([palette, text], [5, 8], [True, False], [None, 0, 3]):
            opts = ['--color-map', color_map, '--bit-depth', str(bits)]
            if color_map == text:
                opts += ['--color-count', '2']
            if not dither:
                opts += ['--no-dither']
            if nodata is not None:
                opts += ['--output-nodata', str(nodata)]
            cases.append((f'mapping {os.path.basename(color_map)} bits={bits} dither={dither} nodata={nodata}', rgb, opts))
        for name, source, opts in cases:
            reference, actual = (os.path.join(tmp, p + '.tif') for p in ['reference', 'actual'])
            try:
                run(['gdal', 'raster', 'rgb-to-palette', source, reference, '--overwrite', *opts])
                run([args.terrio, 'rgb-to-palette', source, actual, '--overwrite', *opts])
                result = check(reference, actual)
            except Exception as exc:
                result = {'error': str(exc)[-800:]}
            result['name'] = name
            results.append(result)
        indexed_source = os.path.join(tmp, 'indexed.tif')
        indexed_error = None
        try:
            run(['gdal', 'raster', 'rgb-to-palette', sources['nodata'], indexed_source])
        except Exception as exc:
            indexed_error = str(exc)[-800:]
        for source, preparation_error in [(palette, None), (indexed_source, indexed_error)]:
            for rgba in [False, True]:
                try:
                    if preparation_error:
                        raise RuntimeError(preparation_error)
                    gdal.Translate(reference, source, rgbExpand='rgba' if rgba else 'rgb')
                    run([args.terrio, 'palette-to-rgb', source, actual, '--overwrite', *(['--rgba'] if rgba else [])])
                    result = check(reference, actual)
                except Exception as exc:
                    result = {'error': str(exc)[-800:]}
                result['name'] = f'expand {os.path.basename(source)} rgba={rgba}'
                results.append(result)
        # Attach reference: SetRasterColorTable replaces the table in-process, retaining indices and nodata.
        try:
            if indexed_error:
                raise RuntimeError(indexed_error)
            ds = gdal.Translate(reference, indexed_source)
            table_source = gdal.Open(palette)
            ds.GetRasterBand(1).SetRasterColorTable(table_source.GetRasterBand(1).GetColorTable())
            ds = None
            run([args.terrio, 'attach-palette', palette, indexed_source, actual, '--overwrite'])
            result = check(reference, actual)
        except Exception as exc:
            result = {'error': str(exc)[-800:]}
        result['name'] = 'attach OPERA palette to Landsat indices'
        results.append(result)
    if args.json:
        print(json.dumps(results, indent=1))
    else:
        for result in results:
            print(format_row(result), {k: v for k, v in result.items() if k.startswith('same_') and v is False})
    return int(any('error' in r or r.get('differing') or any(v is False for k, v in r.items() if k.startswith('same_')) for r in results))


if __name__ == '__main__':
    sys.exit(main())
