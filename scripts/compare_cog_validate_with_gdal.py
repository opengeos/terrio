#!/usr/bin/env python3
"""Compare COG validation verdicts, finding counts and offsets with GDAL 3.13."""
import argparse
import json
import subprocess
import tempfile
from pathlib import Path

import numpy as np
from osgeo import gdal
from osgeo_utils.samples.validate_cloud_optimized_geotiff import validate

gdal.UseExceptions()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('data', type=Path)
    parser.add_argument('--terrio', default='target/release/terrio')
    parser.add_argument('--json', action='store_true')
    args = parser.parse_args()
    results = []
    with tempfile.TemporaryDirectory(prefix='terrio-cog-validation-') as directory:
        root = Path(directory)
        cases = []
        for source, bands in [('landsat.tif', [1, 2, 3]), ('USGS_1_n40w106.tif', [1])]:
            source_path = args.data / source
            if not source_path.exists():
                raise FileNotFoundError(source_path)
            for interleave in ['PIXEL', 'BAND', 'TILE']:
                for big in ['NO', 'YES']:
                    name = f'{source_path.stem}-{interleave}-bigtiff-{big}'
                    path = root / (name+'.tif')
                    gdal.Translate(str(path), str(source_path), format='COG', srcWin=[0, 0, 1024, 768], bandList=bands,
                                   creationOptions=['COMPRESS=DEFLATE', 'BLOCKSIZE=128', 'INTERLEAVE='+interleave, 'BIGTIFF='+big])
                    cases.append((name, path))
        base = cases[0][1]
        masked = root / 'masked-source.tif'
        ds = gdal.Translate(str(masked), str(base), format='GTiff', creationOptions=['TILED=YES'])
        ds.CreateMaskBand(gdal.GMF_PER_DATASET)
        mask = np.full((ds.RasterYSize, ds.RasterXSize), 255, dtype='uint8')
        mask[:128, :128] = 0
        ds.GetRasterBand(1).GetMaskBand().WriteArray(mask)
        ds = None
        for interleave in ['PIXEL', 'BAND', 'TILE']:
            path = root / ('mask-'+interleave+'.tif')
            gdal.Translate(str(path), str(masked), format='COG', creationOptions=['COMPRESS=DEFLATE', 'BLOCKSIZE=128', 'INTERLEAVE='+interleave])
            cases.append(('internal-mask-'+interleave, path))
        ds = gdal.Open(str(base))
        band = ds.GetRasterBand(1)
        offset = int(band.GetMetadataItem('BLOCK_OFFSET_0_0', 'TIFF'))
        count = int(band.GetMetadataItem('BLOCK_SIZE_0_0', 'TIFF'))
        raw = base.read_bytes()
        ds = None
        for name, position in [('bad-leader', offset-4), ('bad-trailer', offset+count)]:
            damaged = bytearray(raw)
            damaged[position] ^= 1
            path = root / (name+'.tif')
            path.write_bytes(damaged)
            cases.append((name, path))
        # An ordinary striped TIFF, with and without external overviews.
        for width in [800, 1200]:
            path = root / f'strips-{width}.tif'
            gdal.Translate(str(path), str(base), format='GTiff', width=width, height=600, creationOptions=['COMPRESS=DEFLATE'])
            cases.append((path.stem, path))
        external = root / 'external-overviews.tif'
        gdal.Translate(str(external), str(base), format='GTiff', creationOptions=['TILED=YES'])
        ds = gdal.Open(str(external))
        ds.BuildOverviews('NEAREST', [2, 4])
        ds = None
        cases.append(('external-overviews', external))
        for name, path in cases:
            for full in [False, True]:
                try:
                    warnings, errors, details = validate(str(path), full_check=full)
                    command = [args.terrio, 'validate-cog', str(path), '--profile', 'gdal', '--full-check', 'yes' if full else 'no', '--json']
                    process = subprocess.run(command, capture_output=True, text=True)
                    report = json.loads(process.stdout)
                    reference = subprocess.run(['gdal', 'driver', 'cog', 'validate', str(path), '--full-check', 'yes' if full else 'no'], capture_output=True)
                    expected = (not errors, len(errors), len(warnings), list(details['ifd_offsets'].values()), list(details['data_offsets'].values()))
                    actual = (report['valid'], len(report['errors']), len(report['warnings']), report['ifd_offsets'], report['data_offsets'])
                    identical = actual == expected and process.returncode == reference.returncode == int(bool(errors))
                    results.append({'name': name+' full='+str(full), 'status': 'identical' if identical else 'differs',
                                    'detail': '' if identical else repr({'expected': expected, 'actual': actual, 'errors': report['errors']})})
                except Exception as error:
                    results.append({'name': name+' full='+str(full), 'status': 'differs', 'detail': str(error)})
    print(json.dumps(results, indent=2) if args.json else '\n'.join(str(result) for result in results))
    return int(any(result['status'] != 'identical' for result in results))


if __name__ == '__main__':
    raise SystemExit(main())
