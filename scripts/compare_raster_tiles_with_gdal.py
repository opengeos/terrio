#!/usr/bin/env python3
"""Compare separate-file raster tiles on real Landsat/USGS inputs with GDAL 3.13."""
import argparse
import json
from pathlib import Path
import subprocess
import tempfile
import threading
import re
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import numpy as np
from osgeo import gdal, osr

gdal.UseExceptions()


def run(args):
    result = subprocess.run([str(a) for a in args], capture_output=True, text=True)
    if result.returncode:
        raise RuntimeError(result.stderr[-1200:])
    return result.stdout


class PixelDifference(AssertionError):
    """A value-only mismatch after every tile's structure/metadata was checked."""


def compare_folders(reference, actual, extension):
    refs = {p.relative_to(reference) for p in reference.rglob('*.' + extension)}
    outs = {p.relative_to(actual) for p in actual.rglob('*.' + extension)}
    if refs != outs:
        raise AssertionError(f'tile names differ: reference-only={refs-outs}, terrio-only={outs-refs}')
    samples = 0
    differences = []
    for name in sorted(refs):
        a, b = (gdal.Open(str(root / name)) for root in (reference, actual))
        x, y = a.ReadAsArray(), b.ReadAsArray()
        if x.shape != y.shape:
            raise AssertionError(f'{name}: shapes {x.shape} != {y.shape}')
        pixels_differ = not np.array_equal(x, y, equal_nan=True)
        if pixels_differ:
            count = np.count_nonzero(~np.isclose(x, y, atol=0, rtol=0, equal_nan=True))
            differences.append(f'{name}: {count}/{x.size} different samples; max delta {np.nanmax(abs(x.astype(float)-y))}')
        samples += x.size
        if extension == 'tif':
            if not np.allclose(a.GetGeoTransform(), b.GetGeoTransform(), atol=1e-8, rtol=1e-12):
                raise AssertionError(f'{name}: geotransforms differ')
            sa, sb = osr.SpatialReference(wkt=a.GetProjection()), osr.SpatialReference(wkt=b.GetProjection())
            if not sa.IsSame(sb):
                raise AssertionError(f'{name}: CRSs differ')
        for i in range(1, a.RasterCount + 1):
            ra, rb = a.GetRasterBand(i), b.GetRasterBand(i)
            na, nb = ra.GetNoDataValue(), rb.GetNoDataValue()
            nodata_equal = na == nb or (na is not None and nb is not None and np.isnan(na) and np.isnan(nb))
            if ra.DataType != rb.DataType or not nodata_equal:
                raise AssertionError(f'{name}: type/nodata differs')
            if not np.array_equal(ra.GetMaskBand().ReadAsArray(), rb.GetMaskBand().ReadAsArray()):
                # Alpha sample differences also change the derived RGB masks.
                alpha_diff = any(
                    a.GetRasterBand(j).GetColorInterpretation() == gdal.GCI_AlphaBand
                    and b.GetRasterBand(j).GetColorInterpretation() == gdal.GCI_AlphaBand
                    and np.array_equal(ra.GetMaskBand().ReadAsArray(), a.GetRasterBand(j).ReadAsArray())
                    and np.array_equal(rb.GetMaskBand().ReadAsArray(), b.GetRasterBand(j).ReadAsArray())
                    and not np.array_equal(a.GetRasterBand(j).ReadAsArray(), b.GetRasterBand(j).ReadAsArray())
                    for j in range(1, a.RasterCount + 1))
                derived_alpha_masks = (ra.GetMaskFlags() & gdal.GMF_ALPHA) and (rb.GetMaskFlags() & gdal.GMF_ALPHA)
                if not (alpha_diff and derived_alpha_masks):
                    raise AssertionError(f'{name}: masks differ')
    if differences:
        raise PixelDifference(f'{len(differences)} tiles differ in values; ' + differences[0])
    return f'{len(refs)} tiles; {samples} exact samples, matching types/nodata/masks and TIFF georeferencing'


def compare_direct_pyramid(source, actual, plan):
    """Independent GDAL warps verify our documented direct-source lower zooms."""
    original = gdal.Open(str(source))
    total = 0
    for level in plan['levels']:
        matrix, limits = level['matrix'], level['range']
        size = matrix['tile_size']
        dx, dy = matrix['resolution']
        for row in range(limits['min_y'], limits['max_y'] + 1):
            for col in range(limits['min_x'], limits['max_x'] + 1):
                left = matrix['origin'][0] + col*size*dx
                top = matrix['origin'][1] - row*size*dy
                reference = gdal.Warp('', original, format='MEM', outputBounds=[left, top-size*dy, left+size*dx, top],
                                      width=size, height=size, resampleAlg='near', dstAlpha=True, errorThreshold=0)
                expected = reference.ReadAsArray()
                if np.all(expected[-1] == 255):
                    expected = expected[:-1]
                tile = gdal.Open(str(actual / str(matrix['zoom']) / str(col) / f'{row}.png'))
                if not np.array_equal(expected, tile.ReadAsArray(), equal_nan=True):
                    raise AssertionError(f'direct-source GDAL warp mismatch at {matrix["zoom"]}/{col}/{row}')
                total += expected.size
    return f'{total} samples exactly match independent GDAL per-tile direct-source warps'


def compare_masked_tiff(source, actual, plan):
    """Cross-check stored masks/data against independent GDAL alpha warps."""
    original = gdal.Open(str(source))
    samples = 0
    for level in plan['levels']:
        matrix, limits = level['matrix'], level['range']
        size = matrix['tile_size']
        dx, dy = matrix['resolution']
        for row in range(limits['min_y'], limits['max_y'] + 1):
            for col in range(limits['min_x'], limits['max_x'] + 1):
                left = matrix['origin'][0] + col*size*dx
                top = matrix['origin'][1] - row*size*dy
                ref = gdal.Warp('', original, format='MEM', outputBounds=[left, top-size*dy, left+size*dx, top],
                                width=size, height=size, resampleAlg='near', dstAlpha=True, errorThreshold=0, overviewLevel='NONE')
                values = ref.ReadAsArray()
                tile = gdal.Open(str(actual / str(matrix['zoom']) / str(col) / f'{row}.tif'))
                expected = values[:-1] if original.RasterCount > 1 else values[0]
                if tile.RasterCount != original.RasterCount or not np.array_equal(expected, tile.ReadAsArray(), equal_nan=True):
                    raise AssertionError('masked GeoTIFF samples/band counts differ from GDAL')
                mask = (values[-1] != 0).astype('uint8')*255
                for band in range(1, tile.RasterCount + 1):
                    a, b = tile.GetRasterBand(band), original.GetRasterBand(band)
                    if a.DataType != b.DataType or a.GetNoDataValue() is not None:
                        raise AssertionError('masked GeoTIFF type/nodata changed')
                    if not a.GetMaskFlags() & gdal.GMF_PER_DATASET or not np.array_equal(a.GetMaskBand().ReadAsArray(), mask):
                        raise AssertionError('dataset mask was not preserved in the GeoTIFF')
                if not np.allclose(tile.GetGeoTransform(), ref.GetGeoTransform(), rtol=1e-12, atol=1e-8):
                    raise AssertionError('masked GeoTIFF grid differs')
                actual_crs, expected_crs = osr.SpatialReference(wkt=tile.GetProjection()), osr.SpatialReference(wkt=ref.GetProjection())
                if not actual_crs.IsSame(expected_crs):
                    raise AssertionError('masked GeoTIFF CRS differs')
                samples += expected.size
    return f'{samples} exact samples; matching GDAL validity, types, grid/CRS with internal dataset masks'


def remote_probe(data, root, terrio, masked=False):
    """Count response payloads for one tile of an actual USGS-derived COG."""
    label = 'remote-masked' if masked else 'remote'
    cog = root / (label + '.tif')
    source = data / 'usgs_dem.tif'
    if masked:
        source = root / 'remote-mask-source.tif'
        ds = gdal.Translate(str(source), str(data / 'usgs_dem.tif'), width=2048, height=2048)
        ds.GetRasterBand(1).DeleteNoDataValue()
        ds.CreateMaskBand(gdal.GMF_PER_DATASET)
        mask = np.full((2048, 2048), 255, dtype='uint8')
        mask[550:580, 400:430] = 0
        ds.GetRasterBand(1).GetMaskBand().WriteArray(mask)
        ds = None
    gdal.Translate(str(cog), str(source), format='COG', width=2048, height=2048,
                   creationOptions=['BLOCKSIZE=128', 'COMPRESS=DEFLATE', 'OVERVIEWS=NONE'])
    size = cog.stat().st_size
    counts = {'bytes': 0, 'requests': 0}

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_HEAD(self):
            if self.path != '/dem.tif':
                self.send_error(404)
                return
            self.send_response(200)
            self.send_header('Content-Length', str(size))
            self.send_header('Accept-Ranges', 'bytes')
            self.end_headers()

        def do_GET(self):
            if self.path != '/dem.tif':
                self.send_error(404)
                return
            match = re.fullmatch(r'bytes=(\d+)-(\d*)', self.headers.get('Range', ''))
            start, end = (0, size-1) if match is None else (int(match[1]), min(int(match[2]) if match[2] else size-1, size-1))
            self.send_response(206 if match else 200)
            self.send_header('Content-Length', str(end-start+1))
            if match:
                self.send_header('Content-Range', f'bytes {start}-{end}/{size}')
            self.end_headers()
            with cog.open('rb') as stream:
                stream.seek(start)
                payload = stream.read(end-start+1)
            self.wfile.write(payload)
            counts['bytes'] += len(payload)
            counts['requests'] += 1

    opts = ['--tiling-scheme', 'raster', '--format', 'GTiff', '--no-alpha', '--tile-size', '128',
            '--max-zoom', '4', '--min-x', '3', '--max-x', '3', '--min-y', '4', '--max-y', '4', '--resampling', 'nearest']
    reference, actual = root / (label + '-ref'), root / (label + '-out')
    if masked:
        plan = json.loads(run([terrio, 'tile', cog, actual, '--dry-run', *opts]))
    else:
        run(['gdal', 'raster', 'tile', cog, reference, '--webviewer', 'none', '--num-threads', '1', *opts])
    with ThreadingHTTPServer(('127.0.0.1', 0), Handler) as server:
        worker = threading.Thread(target=server.serve_forever, daemon=True)
        worker.start()
        try:
            run([terrio, 'tile', f'http://127.0.0.1:{server.server_port}/dem.tif', actual, *opts])
            detail = compare_masked_tiff(cog, actual, plan) if masked else compare_folders(reference, actual, 'tif')
        finally:
            server.shutdown()
            worker.join()
    if not 0 < counts['bytes'] < size/4:
        raise AssertionError(f'single-tile I/O was not bounded: {counts} from {size} bytes')
    return f"{detail}; {counts['bytes']} served bytes in {counts['requests']} GETs from {size}-byte COG"


def compare_pyramid(source, reference, actual, plan):
    """Separate exact generated-pyramid agreement from verified policy differences."""
    direct = compare_direct_pyramid(source, actual, plan)
    try:
        compare_folders(reference, actual, 'png')
    except PixelDifference as difference:
        return 'tolerance', direct + '; documented generated-pyramid difference: ' + str(difference)
    return 'identical', direct + '; GDAL generated pyramid also matches'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('data', type=Path)
    parser.add_argument('--terrio', default='target/release/terrio')
    parser.add_argument('--json', action='store_true')
    args = parser.parse_args()
    results = []
    with tempfile.TemporaryDirectory(prefix='terrio-folder-tiles-') as directory:
        root = Path(directory)
        rgb = root / 'landsat-rgb.tif'
        ds = gdal.Translate(str(rgb), str(args.data / 'landsat.tif'), srcWin=[600, 500, 513, 299], bandList=[1, 2, 3], outputType=gdal.GDT_Byte)
        # Isolate mask/alpha policies from the original image's inherited nodata.
        for i in range(1, 4):
            ds.GetRasterBand(i).DeleteNoDataValue()
        ds = None
        dem = root / 'usgs.tif'
        ds = gdal.Translate(str(dem), str(args.data / 'usgs_dem.tif'), srcWin=[600, 500, 513, 299])
        ds = None
        sources = {'rgb': rgb, 'dem': dem}
        for kind in ['nodata', 'partial-nodata', 'mask', 'alpha']:
            path = root / f'{kind}.tif'
            ds = gdal.Translate(str(path), str(rgb), bandList=[1, 2, 3, 1] if kind == 'alpha' else [1, 2, 3])
            valid = np.full((299, 513), 255, dtype='uint8')
            valid[:25, :30] = 0
            if kind in ['nodata', 'partial-nodata']:
                for i in range(1, 4):
                    values = ds.GetRasterBand(i).ReadAsArray()
                    nodata = 255 if kind == 'partial-nodata' else 0
                    if kind == 'nodata' or i == 1:
                        values[:25, :30] = nodata
                    ds.GetRasterBand(i).WriteArray(values)
                    ds.GetRasterBand(i).SetNoDataValue(nodata)
            elif kind == 'mask':
                ds.CreateMaskBand(gdal.GMF_PER_DATASET)
                ds.GetRasterBand(1).GetMaskBand().WriteArray(valid)
            else:
                ds.GetRasterBand(4).SetColorInterpretation(gdal.GCI_AlphaBand)
                ds.GetRasterBand(4).WriteArray(valid)
            ds = None
            sources[kind] = path
        blank = root / 'all-invalid.tif'
        ds = gdal.Translate(str(blank), str(rgb))
        for i in range(1, 4):
            ds.GetRasterBand(i).Fill(0)
            ds.GetRasterBand(i).SetNoDataValue(0)
        ds = None
        sources['blank'] = blank
        cases = []
        for source in sources:
            for convention in ['xyz', 'tms']:
                for size in [128, 256]:
                    fmt = 'GTiff' if source == 'dem' else 'PNG'
                    cases.append((source, convention, size, fmt, 'raster', 2, []))
        for kind, value in [('dem-blank', -999999.0), ('dem-zero', 0.0)]:
            path = root / f'{kind}.tif'
            ds = gdal.Translate(str(path), str(dem))
            ds.GetRasterBand(1).SetNoDataValue(-999999.0)
            ds.GetRasterBand(1).Fill(value)
            ds = None
            sources[kind] = path
            cases.append((kind, 'xyz', 128, 'GTiff', 'raster', 2, ['--skip-blank']))
        # Interior nonzero nodata is zero-filled by GDAL's no-alpha tile writer,
        # even while its original nodata tag is retained.
        path = root / 'dem-interior-nodata.tif'
        ds = gdal.Translate(str(path), str(dem))
        values = ds.GetRasterBand(1).ReadAsArray()
        values[:25, :30] = -9999
        ds.GetRasterBand(1).WriteArray(values)
        ds.GetRasterBand(1).SetNoDataValue(-9999)
        ds = None
        sources['dem-interior-nodata'] = path
        cases.append(('dem-interior-nodata', 'xyz', 128, 'GTiff', 'raster', 2, []))
        for source in ['nodata', 'alpha']:
            cases.append((source, 'xyz', 128, 'PNG', 'raster', 2, ['--no-alpha']))
        # GDAL's alpha scaling for UInt32/Int32 is 255 (not the type maximum).
        for kind, dtype, alpha in [('alpha-uint32', gdal.GDT_UInt32, 2147483648), ('alpha-int32', gdal.GDT_Int32, 1073741824)]:
            path = root / f'{kind}.tif'
            original = gdal.Open(str(rgb))
            ds = gdal.GetDriverByName('GTiff').Create(str(path), original.RasterXSize, original.RasterYSize, 4, dtype)
            ds.SetGeoTransform(original.GetGeoTransform())
            ds.SetProjection(original.GetProjection())
            for band, interpretation in enumerate([gdal.GCI_RedBand, gdal.GCI_GreenBand, gdal.GCI_BlueBand], 1):
                ds.GetRasterBand(band).WriteArray(original.GetRasterBand(band).ReadAsArray())
                ds.GetRasterBand(band).SetColorInterpretation(interpretation)
            ds.GetRasterBand(4).Fill(alpha)
            ds.GetRasterBand(4).SetColorInterpretation(gdal.GCI_AlphaBand)
            ds = None
            sources[kind] = path
            cases.append((kind, 'xyz', 128, 'GTiff', 'raster', 2, ['--add-alpha']))
        # Global coordinate planning and reprojected nearest-neighbor pixels.
        for scheme in ['WebMercatorQuad', 'WorldCRS84Quad']:
            for convention in ['xyz', 'tms']:
                cases.append(('rgb', convention, 128, 'PNG', scheme, 6, []))
        cases.append(('rgb', 'xyz', 128, 'PNG', 'raster', 2, ['--add-alpha']))
        cases.append(('blank', 'xyz', 128, 'PNG', 'raster', 2, ['--skip-blank']))
        cases.append(('rgb', 'xyz', 128, 'PNG', 'raster', 2, ['pyramid']))
        # No-alpha output and coordinate restriction policies.
        cases.append(('rgb', 'xyz', 128, 'PNG', 'raster', 2, ['--no-alpha']))
        cases.append(('rgb', 'xyz', 128, 'PNG', 'raster', 2, ['--min-x', '1', '--max-x', '1', '--min-y', '1', '--max-y', '1']))
        for index, (source, convention, size, fmt, scheme, zoom, extra) in enumerate(cases):
            name = f'{source} {scheme} {convention} size={size} zoom={zoom} {" ".join(extra)}'.strip()
            reference, actual = root / f'ref{index}', root / f'out{index}'
            opts = ['--tiling-scheme', scheme, '--max-zoom', str(zoom), '--min-zoom', str(0 if 'pyramid' in extra else zoom), '--tile-size', str(size),
                    '--convention', convention, '--format', fmt, '--resampling', 'nearest', *[v for v in extra if v != 'pyramid']]
            if fmt == 'GTiff' and '--add-alpha' not in opts:
                opts += ['--no-alpha']
            try:
                run(['gdal', 'raster', 'tile', sources[source], reference, '--webviewer', 'none', '--num-threads', '1', *opts])
                run([args.terrio, 'tile', sources[source], actual, *opts])
                if source == 'dem-blank':
                    # GDAL 3.13.3 --no-alpha loses source validity before skip-blank.
                    original = gdal.Open(str(sources[source]))
                    if np.any(original.GetRasterBand(1).GetMaskBand().ReadAsArray()):
                        raise AssertionError('nodata fixture is not fully invalid')
                    if list(actual.rglob('*.tif')):
                        raise AssertionError('Terrio failed to omit a fully invalid tile')
                    refs = list(reference.rglob('*.tif'))
                    if len(refs) != 15 or any(np.any(gdal.Open(str(p)).ReadAsArray()) for p in refs):
                        raise AssertionError('GDAL no-alpha skip-blank behavior changed; re-audit this correction')
                    results.append(dict(name=name, status='tolerance', detail='Terrio omits all 15 fully invalid tiles; GDAL 3.13.3 --no-alpha retains 15 zero-filled tiles. Source validity and both outcomes independently checked.'))
                elif 'pyramid' in extra:
                    plan = json.loads(run([args.terrio, 'tile', sources[source], actual, '--dry-run', *opts]))
                    status, detail = compare_pyramid(sources[source], reference, actual, plan)
                    results.append(dict(name=name, status=status, detail=detail))
                else:
                    detail = compare_folders(reference, actual, 'tif' if fmt == 'GTiff' else 'png')
                    results.append(dict(name=name, status='identical', detail=detail))
            except Exception as error:
                results.append(dict(name=name, status='differs', detail=str(error)))
        for label, source, dtype in [('RGB UInt16', rgb, gdal.GDT_UInt16), ('DEM Float32', dem, gdal.GDT_Float32)]:
            try:
                masked = root / ('masked-' + label.replace(' ', '-') + '.tif')
                ds = gdal.Translate(str(masked), str(source), outputType=dtype)
                for band in range(1, ds.RasterCount + 1):
                    ds.GetRasterBand(band).DeleteNoDataValue()
                ds.CreateMaskBand(gdal.GMF_PER_DATASET)
                mask = np.full((299, 513), 255, dtype='uint8')
                mask[:25, :30] = 0
                ds.GetRasterBand(1).GetMaskBand().WriteArray(mask)
                ds = None
                actual = root / ('masked-out-' + label.replace(' ', '-'))
                opts = ['--tiling-scheme', 'raster', '--format', 'GTiff', '--max-zoom', '2', '--tile-size', '128', '--resampling', 'nearest']
                run([args.terrio, 'tile', masked, actual, *opts])
                plan = json.loads(run([args.terrio, 'tile', masked, actual, '--dry-run', *opts]))
                detail = compare_masked_tiff(masked, actual, plan)
                results.append(dict(name='masked GeoTIFF ' + label, status='identical', detail=detail))
            except Exception as error:
                results.append(dict(name='masked GeoTIFF ' + label, status='differs', detail=str(error)))
        try:
            results.append(dict(name='remote USGS single-tile I/O', status='identical', detail=remote_probe(args.data, root, args.terrio)))
        except Exception as error:
            results.append(dict(name='remote USGS single-tile I/O', status='differs', detail=str(error)))
        try:
            results.append(dict(name='remote masked USGS single-tile I/O', status='identical', detail=remote_probe(args.data, root, args.terrio, masked=True)))
        except Exception as error:
            results.append(dict(name='remote masked USGS single-tile I/O', status='differs', detail=str(error)))
    print(json.dumps(results, indent=2) if args.json else '\n'.join(f'{r["status"]}: {r["name"]}: {r["detail"]}' for r in results))
    return int(any(r['status'] == 'differs' for r in results))


if __name__ == '__main__':
    raise SystemExit(main())
