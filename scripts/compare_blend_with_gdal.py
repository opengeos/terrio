#!/usr/bin/env python3
"""Compare every raster blend operator with GDAL 3.13 on real Landsat pixels."""
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
OPERATORS = ('src-over', 'hsv-value', 'multiply', 'screen', 'overlay', 'hard-light',
             'darken', 'lighten', 'color-dodge', 'color-burn')


def prepare(data, tmp):
    """Real Landsat colors with controlled gray/alpha/boundary samples."""
    src = gdal.Translate('', os.path.join(data, 'landsat.tif'), format='MEM',
                         srcWin=[600, 500, 33, 17], bandList=[1, 2, 3],
                         outputType=gdal.GDT_Byte, scaleParams=[[0, 255, 0, 255]])
    rgb = src.ReadAsArray()
    paths = {}
    for n, reverse in itertools.product(range(1, 5), [False, True]):
        path = os.path.join(tmp, f'{n}_{reverse}.tif')
        ds = gdal.GetDriverByName('GTiff').Create(path, 33, 17, n, gdal.GDT_Byte)
        ds.SetGeoTransform(src.GetGeoTransform())
        ds.SetProjection(src.GetProjection())
        for b in range(n):
            values = rgb[b % 3].copy()
            if reverse:
                values = 255 - values[:, ::-1]
            # Exercise byte edges, zero alpha, and arbitrary fractional alpha.
            values.flat[:9] = [0, 1, 2, 63, 127, 128, 254, 255, 0]
            if n in (2, 4) and b == n - 1:
                values = np.resize(np.array([0, 1, 63, 127, 128, 200, 254, 255], dtype='uint8'), values.shape)
            band = ds.GetRasterBand(b + 1)
            band.WriteArray(values)
            names = ['Gray', 'Alpha'] if n <= 2 else ['Red', 'Green', 'Blue', 'Alpha']
            band.SetColorInterpretation(gdal.GetColorInterpretationByName(names[b]))
        ds = None
        paths[n, reverse] = path
    palette = os.path.join(tmp, 'palette.tif')
    ds = gdal.Translate(palette, os.path.join(data, 'OPERA_L3_DSWx_WTR.tif'),
                        width=33, height=17)
    band = ds.GetRasterBand(1)
    values = band.ReadAsArray()
    values.flat[:3] = [0, 1, 255]
    band.WriteArray(values)
    ds = None
    paths['palette-tiff'] = palette
    paths['palette-png'] = os.path.join(tmp, 'palette.png')
    gdal.Translate(paths['palette-png'], palette, format='PNG')
    return paths


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument('data')
    ap.add_argument('--terrio', default='target/release/terrio')
    ap.add_argument('--json', action='store_true')
    ap.add_argument('--only')
    a = ap.parse_args()
    if int(gdal.VersionInfo("VERSION_NUM")) < 3130000:
        ap.error("the complete blend suite requires GDAL >= 3.13")
    results = []
    with tempfile.TemporaryDirectory(prefix='terrio-blend-') as tmp:
        paths = prepare(a.data, tmp)
        matrix = [(*c, None) for c in itertools.product(OPERATORS, range(1, 5), range(1, 5), [0, 37, 100])]
        matrix += [(op, 4, 4, opacity, kind) for op, opacity, kind in itertools.product(
            ['src-over', 'multiply'], [0, 37, 100], ['palette-tiff', 'palette-png'])]
        for op, nb, ns, opacity, kind in matrix:
            if op == 'hsv-value' and nb < 3:
                continue
            if op in ('lighten', 'darken') and (nb + 1) // 2 != (ns + 1) // 2:
                continue
            name = f'{op} {kind or nb}/{ns} opacity={opacity}'
            if a.only and a.only not in name:
                continue
            base, overlay = paths[kind] if kind else paths[nb, False], paths[ns, True]
            gout, tout = (os.path.join(tmp, k + '.tif') for k in ('gdal', 'terrio'))
            try:
                args = [base, overlay, '{out}', '--operator', op, '--opacity', str(opacity)]
                # Normalize combinations for which GDAL 3.13.3 has unsafe
                # band-count handling (including its per-band fast path).
                # RGBA expansion preserves the channel/alpha interpretation;
                # selecting the base channels below restores the output shape.
                normalized = (op == 'src-over' and (nb == 2 or ns == 2)) or (
                    nb < ns and op not in ('multiply', 'screen', 'overlay', 'hard-light'))
                rb, ro = base, overlay
                if normalized:
                    def rgba(path, n, gray=False):
                        ds = gdal.Open(path)
                        bands = [1, 1, 1, 2] if n == 2 else [1, 1, 1] if n == 1 else list(range(1, n + 1))
                        expanded = gdal.Translate('', ds, format='MEM', bandList=bands)
                        if gray:
                            rgb = ds.ReadAsArray().astype('int32')
                            value = ((rgb[0]*306 + rgb[1]*601 + rgb[2]*117)//1024).astype('uint8')
                            for b in range(1, 4):
                                expanded.GetRasterBand(b).WriteArray(value)
                        if n in (1, 3):
                            full = gdal.GetDriverByName('MEM').Create('', 33, 17, 4, gdal.GDT_Byte)
                            full.SetGeoTransform(ds.GetGeoTransform())
                            full.SetProjection(ds.GetProjection())
                            for b in range(1, 4):
                                full.GetRasterBand(b).WriteArray(expanded.GetRasterBand(b).ReadAsArray())
                            full.GetRasterBand(4).Fill(255)
                            return full
                        return expanded
                    rb, ro = rgba(base, nb), rgba(overlay, ns, op == "src-over" and nb <= 2 and ns >= 3)
                    for ds, entry_name in [(rb, f'base_rgba_{len(results)}.tif'), (ro, f'overlay_rgba_{len(results)}.tif')]:
                        gdal.GetDriverByName('GTiff').CreateCopy(os.path.join(tmp, entry_name), ds)
                    rb, ro = os.path.join(tmp, f'base_rgba_{len(results)}.tif'), os.path.join(tmp, f'overlay_rgba_{len(results)}.tif')
                if op not in ('src-over', 'hsv-value'):
                    # Generic per-band kernels in GDAL 3.13.3 process only the
                    # first sample. Use its correct whole-dataset kernels here.
                    algorithm = gdal.Run('raster', 'blend', input=rb, overlay=ro,
                                         output_format='MEM', operator=op, opacity=opacity)
                    ref = algorithm.Output()
                else:
                    pipeline = os.path.join(tmp, f'blend_{len(results)}.gdalg.json')
                    with gdal.Run('raster', 'blend', input=rb, overlay=ro, output=pipeline,
                                  output_format='GDALG', overwrite=True, operator=op, opacity=opacity):
                        pass
                    ref = gdal.Open(pipeline)
                selected = ([1, 4] if nb == 2 else list(range(1, nb + 1))) if normalized else list(range(1, ref.RasterCount + 1))
                dst = gdal.GetDriverByName('GTiff').Create(gout, 33, 17, len(selected), gdal.GDT_Byte)
                dst.SetGeoTransform(ref.GetGeoTransform())
                dst.SetProjection(ref.GetProjection())
                for b, src_band in enumerate(selected, 1):
                    dst.GetRasterBand(b).WriteArray(ref.GetRasterBand(src_band).ReadAsArray())
                    dst.GetRasterBand(b).SetColorInterpretation(
                        gdal.GCI_GrayIndex if len(selected) <= 2 and b == 1 else ref.GetRasterBand(src_band).GetColorInterpretation())
                dst = ref = None
                run([a.terrio, 'blend'] + [s.replace('{out}', tout) for s in args])
                r = compare(gout, tout, 0)
                r.update(raster_extra(gout, tout))
            except Exception as exc:
                r = {'error': str(exc)[-500:]}
            r['name'] = name
            results.append(r)
            if not a.json:
                print(format_row(r), flush=True)
    if a.json:
        print(json.dumps(results, indent=1))
    return int(any('error' in r or r.get('differing') or any(r.get(k) is False for k in
                   ('same_size', 'same_type', 'same_nodata', 'same_crs', 'same_metadata', 'same_georeferencing')) for r in results))


if __name__ == '__main__':
    sys.exit(main())
