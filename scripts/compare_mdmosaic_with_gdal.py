#!/usr/bin/env python3
"""Compare coordinate-aligned multidimensional mosaics on real NOAA wind values."""
import argparse
import json
import subprocess
import tempfile
from pathlib import Path

import numpy as np
from osgeo import gdal

gdal.UseExceptions()


def create(path, axes, values, dtype=gdal.GDT_Float32, nested=False, coordinate_dtype=gdal.GDT_Float64, bounds=False, coordinate_ranges=False, missing_value_only=False, extra_payload=False, auxiliary=False):
    dataset = gdal.GetDriverByName('netCDF').CreateMultiDimensional(str(path))
    root = dataset.GetRootGroup()
    dimensions = []
    for name, coordinates in axes:
        dimension = root.CreateDimension(name, None, None, len(coordinates))
        variable = root.CreateMDArray(name, [dimension], gdal.ExtendedDataType.Create(coordinate_dtype))
        variable.Write(np.asarray(coordinates, dtype='<f4' if coordinate_dtype == gdal.GDT_Float32 else '<f8').tobytes())
        if coordinate_ranges:
            variable.CreateAttribute('actual_range', [2], gdal.ExtendedDataType.Create(gdal.GDT_Float64)).Write([float(min(coordinates)), float(max(coordinates))])
        dimensions.append(dimension)
    group = root.CreateGroup('science') if nested else root
    variable = group.CreateMDArray('wind', dimensions, gdal.ExtendedDataType.Create(dtype))
    names = {'Byte': '<u1', 'Int8': '<i1', 'UInt16': '<u2', 'Int16': '<i2', 'UInt32': '<u4',
             'Int32': '<i4', 'UInt64': '<u8', 'Int64': '<i8', 'Float32': '<f4', 'Float64': '<f8'}
    values = np.asarray(values, dtype=names[gdal.GetDataTypeName(dtype)])
    fill = 255 if dtype == gdal.GDT_Byte else 0 if dtype in (gdal.GDT_UInt16, gdal.GDT_UInt32, gdal.GDT_UInt64) else -99
    if missing_value_only:
        variable.CreateAttribute("missing_value", [], gdal.ExtendedDataType.Create(dtype)).Write(fill)
    else:
        variable.SetNoDataValueDouble(fill)
    variable.Write(values.tobytes())
    variable.CreateAttribute('units', [], gdal.ExtendedDataType.CreateString()).Write('m/s')
    if auxiliary:
        y, x = (np.asarray(coordinates) for _, coordinates in axes[-2:])
        shape = (len(y), len(x))
        for name, data in [("latitude2d", np.broadcast_to(y[:,None],shape)), ("longitude2d", np.broadcast_to(x[None,:],shape))]:
            coordinate = group.CreateMDArray(name, dimensions[-2:], gdal.ExtendedDataType.Create(gdal.GDT_Float64))
            coordinate.Write(np.asarray(data,dtype='<f8').tobytes())
            coordinate.CreateAttribute("standard_name", [], gdal.ExtendedDataType.CreateString()).Write("latitude" if name == "latitude2d" else "longitude")
            coordinate.CreateAttribute("units", [], gdal.ExtendedDataType.CreateString()).Write("degrees_north" if name == "latitude2d" else "degrees_east")
        variable.CreateAttribute("coordinates", [], gdal.ExtendedDataType.CreateString()).Write("latitude2d longitude2d")
    if extra_payload:
        extra = group.CreateMDArray("unselected", dimensions, gdal.ExtendedDataType.Create(dtype))
        extra.Write((values + 17).tobytes())
    if bounds:
        bound_dim = root.CreateDimension('bnds', None, None, 2)
        bound = root.CreateMDArray('lat_bnds', [dimensions[0], bound_dim], gdal.ExtendedDataType.Create(gdal.GDT_Float64))
        root.OpenMDArray(axes[0][0]).CreateAttribute('bounds', [], gdal.ExtendedDataType.CreateString()).Write('lat_bnds')
        bound.Write(np.asarray([[float(value)-0.25, float(value)+0.25] for value in axes[0][1]], dtype='<f8').tobytes())
    dataset.FlushCache()


def arrays(group):
    result = {}
    for name in group.GetMDArrayNames() or []:
        array = group.OpenMDArray(name)
        result[array.GetFullName()] = array
    for name in group.GetGroupNames() or []:
        result.update(arrays(group.OpenGroup(name)))
    return result


def comparison(reference, actual, allow_float32_coordinates=False, source_coordinates=None, expected_nested=False, excluded_arrays=(), expected_auxiliary=False):
    ref = gdal.OpenEx(str(reference), gdal.OF_MULTIDIM_RASTER)
    out = gdal.OpenEx(str(actual), gdal.OF_MULTIDIM_RASTER)
    ra, oa = arrays(ref.GetRootGroup()), arrays(out.GetRootGroup())
    reference_paths, actual_paths = dict(ra), dict(oa)
    assert not any(name.rsplit('/',1)[-1] in excluded_arrays for name in oa), sorted(oa)
    ra = {name: array for name,array in ra.items() if name.rsplit('/',1)[-1] not in excluded_arrays}
    def resolved_path(known, owner, name):
        assert isinstance(name,str) and name
        if name.startswith('/'):
            assert name in known, (owner,name)
            return name
        parent = owner.rsplit('/',1)[0] or '/'
        while True:
            candidate = parent.rstrip('/') + '/' + name
            if candidate in known:
                return candidate
            assert parent != '/', (owner,name)
            parent = parent.rsplit('/',1)[0] or '/'
    path_difference = ''
    if set(ra) != set(oa):
        assert expected_nested, (sorted(ra),sorted(oa))
        # GDAL places a selected nested array at the root; Terrio retains its
        # source group path. Require an unambiguous one-to-one name mapping.
        assert len(ra) == len(oa)
        mapped = {}
        for name in ra:
            candidates = [key for key in oa if key.rsplit('/', 1)[-1] == name.rsplit('/', 1)[-1]]
            assert len(candidates) == 1, (name, candidates)
            mapped[name] = oa[candidates[0]]
        assert len({array.GetFullName() for array in mapped.values()}) == len(oa)
        oa = mapped
        path_difference = 'Terrio preserves source group paths; GDAL places selected nested arrays in the root group'
    count = differing = within_count = 0
    max_diff = 0
    permitted_type_difference = False
    source_coordinate_adjusted = False
    unexpected_type_difference = False
    same_type = same_nodata = same_metadata = same_size = True
    for name, r in ra.items():
        a = oa[name]
        if expected_auxiliary and r.GetName() == 'wind':
            names = {coordinate.GetFullName() for coordinate in a.GetCoordinateVariables()}
            assert {'/latitude2d','/longitude2d'} <= names, names
        if expected_nested and r.GetName() == 'wind':
            assert a.GetFullName() == '/science/wind', a.GetFullName()
        rv, av = r.ReadAsArray(), a.ReadAsArray()
        # The descending-axis case normalizes GDAL's inputs (3.13.3 writes an
        # invalid zero-sized VRT dimension otherwise), then restores Terrio's
        # source-axis orientation by exact coordinate matching.
        for axis, (rd, ad) in enumerate(zip(r.GetDimensions(), a.GetDimensions())):
            rc, ac = rd.GetIndexingVariable(), ad.GetIndexingVariable()
            if rc is not None and ac is not None:
                rcv, acv = rc.ReadAsArray(), ac.ReadAsArray()
                if allow_float32_coordinates and acv.dtype == np.dtype('float32'):
                    rcv = rcv.astype('float32')
                if not np.array_equal(rcv, acv):
                    assert len(rcv) == len(acv)
                    # Reorder only a true permutation. A coordinate mismatch
                    # remains in place for the numerical comparison below.
                    matches = [np.flatnonzero(rcv == value) for value in acv]
                    if all(len(match) == 1 for match in matches):
                        order = [int(match[0]) for match in matches]
                        if len(set(order)) == len(order):
                            rv = np.take(rv, order, axis=axis)
        rt, at = r.GetDataType().GetNumericDataType(), a.GetDataType().GetNumericDataType()
        rd = r.GetDimensions()
        coordinate = len(rd) == 1 and rd[0].GetIndexingVariable() is not None and rd[0].GetIndexingVariable().GetFullName() == r.GetFullName()
        permitted = coordinate and allow_float32_coordinates and rt == gdal.GDT_Float64 and at == gdal.GDT_Float32
        same_type &= rt == at
        permitted_type_difference |= permitted
        unexpected_type_difference |= rt != at and not permitted
        same_size &= rv.shape == av.shape
        assert rv.shape == av.shape, (name, rv.shape, av.shape)
        different = (rv != av) & ~(np.isnan(rv) & np.isnan(av))
        accepted = np.zeros(rv.shape, dtype=bool)
        if coordinate and source_coordinates is not None:
            # GDAL regenerates a regular grid from one estimated step. Preserve
            # source samples exactly, allowing its rounding drift only where an
            # independently supplied source coordinate proves the desired value.
            expected = rv.astype(av.dtype).copy()
            source_samples = np.zeros(rv.shape,dtype=bool)
            for values in source_coordinates.get(r.GetName(), []):
                values = np.asarray(values, dtype=av.dtype)
                start = int(np.argmin(np.abs(rv - values[0])))
                assert start + len(values) <= len(expected)
                expected[start:start + len(values)] = values
                source_samples[start:start + len(values)] = True
            accepted = expected == av
            different |= source_samples & ~accepted
        elif permitted:
            accepted = rv.astype('float32') == av
        differing += int(different.sum())
        source_coordinate_adjusted |= coordinate and bool((different & accepted).any())
        within_count += int((~different | accepted).sum())
        count += rv.size
        for left, right in zip(rv[different], av[different]):
            if np.issubdtype(rv.dtype, np.integer) and np.issubdtype(av.dtype, np.integer):
                difference = abs(int(left)-int(right))
            else:
                difference = abs(float(left)-float(right))
            max_diff = max(max_diff, difference if np.isfinite(difference) else float('inf'))
        rn, an = r.GetNoDataValue(), a.GetNoDataValue()
        same_nodata &= rn == an or (rn is not None and an is not None and np.isnan(rn) and np.isnan(an))
        same_metadata &= [d.GetName() for d in r.GetDimensions()] == [d.GetName() for d in a.GetDimensions()]
        same_metadata &= r.GetUnit() == a.GetUnit()
        rat = {at.GetName(): at.Read() for at in r.GetAttributes()}
        aat = {at.GetName(): at.Read() for at in a.GetAttributes()}
        assert 'actual_range' not in aat, (name, aat.get('actual_range'))
        for key in ['scale_factor', 'add_offset', 'units']:
            same_metadata &= rat.get(key) == aat.get(key)
        if rat.get('bounds') is not None and aat.get('bounds') is not None:
            target = resolved_path(reference_paths,r.GetFullName(),rat['bounds'])
            actual_target = resolved_path(actual_paths,a.GetFullName(),aat['bounds'])
            same_metadata &= target in oa and oa[target].GetFullName() == actual_target
        else:
            same_metadata &= rat.get('bounds') == aat.get('bounds')
    return dict(path_difference=path_difference, same_size=same_size, same_type=same_type, same_nodata=same_nodata, same_metadata=same_metadata,
                differing=differing, exact=1 - differing / count, within_tol=within_count / count, max_diff=max_diff,
                permitted_coordinate_type_difference=permitted_type_difference and not unexpected_type_difference,
                coordinate_difference='Source coordinates retained exactly; reference grid rounding is accepted only where actual input samples prove the expected value, with reference-rounded gap coordinates' if source_coordinate_adjusted or permitted_type_difference else '')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('data', type=Path)
    parser.add_argument('--terrio', default='target/release/terrio')
    parser.add_argument('--json', action='store_true')
    args = parser.parse_args()
    wind = gdal.OpenEx(str(args.data / 'wind_global.nc'), gdal.OF_MULTIDIM_RASTER)
    root = wind.GetRootGroup()
    values = root.OpenMDArray('u_wind').ReadAsArray()[:16, :20]
    lat, lon = (root.OpenMDArray(n).ReadAsArray()[:count] for n, count in [('lat', 16), ('lon', 20)])
    values, lat = values[::-1, :], lat[::-1]
    results = []
    with tempfile.TemporaryDirectory(prefix='terrio-mdmosaic-') as temporary:
        tmp = Path(temporary)
        def case(name, pieces, dtype=gdal.GDT_Float32, nested=False, selected=False, output='nc', normalize_descending=False, coordinate_dtype=gdal.GDT_Float64, bounds=False, coordinate_ranges=False, missing_value_only=False, auxiliary=False):
            try:
                sources = []
                for index, (axes, data) in enumerate(pieces):
                    source = tmp / f'source-{index}.nc'
                    create(source, axes, data, dtype, nested, coordinate_dtype, bounds, coordinate_ranges, missing_value_only, extra_payload=selected, auxiliary=auxiliary)
                    sources.append(str(source))
                reference_sources = sources
                if normalize_descending:
                    reference_sources = []
                    for index, (axes, data) in enumerate(pieces):
                        normalized = []
                        for axis, (dimension, coordinates) in enumerate(axes):
                            if len(coordinates) > 1 and coordinates[1] < coordinates[0]:
                                coordinates = coordinates[::-1]
                                data = np.flip(data, axis=axis)
                            normalized.append((dimension, coordinates))
                        source = tmp / f'reference-source-{index}.nc'
                        create(source, normalized, data, dtype, nested, coordinate_dtype, bounds, coordinate_ranges, missing_value_only)
                        reference_sources.append(str(source))
                reference = tmp / 'reference.vrt'
                reference.unlink(missing_ok=True)
                actual = tmp / ('actual-' + str(len(results)) + '.' + output)
                selection = ['--array', '/science/wind' if nested else 'wind'] if selected else []
                subprocess.run(['gdal', 'mdim', 'mosaic', *reference_sources, str(reference), *(selection if not ((bounds or auxiliary) and selected) else [])], capture_output=True, check=True)
                subprocess.run([args.terrio, 'mdmosaic', *sources, str(actual), *selection], capture_output=True, check=True)
                source_coordinates = {}
                for axes, _ in pieces:
                    for axis_name, coordinates in axes:
                        source_coordinates.setdefault(axis_name, []).append(coordinates)
                result = comparison(reference, actual, coordinate_dtype == gdal.GDT_Float32, source_coordinates, expected_nested=nested, excluded_arrays=("unselected",) if selected else (), expected_auxiliary=auxiliary)
                if (bounds or auxiliary) and selected:
                    result['reference_normalization'] = 'GDAL selected-array output drops referenced CF dependencies; compare its complete output excluding the explicitly unselected payload'
                if normalize_descending:
                    result['reference_normalization'] = 'GDAL 3.13.3 descending-axis VRT size/offset bug; reference inputs flipped and output coordinate order restored'
            except subprocess.CalledProcessError as error:
                result = {'error': error.stderr.decode()[-800:]}
            except Exception as error:
                result = {'error': str(error)[-800:]}
            result['name'] = name
            results.append(result)
        tiled = [([('lat', lat[y:y+8]), ('lon', lon[x:x+10])], values[y:y+8, x:x+10]) for y,x in [(0,0),(0,10),(8,0),(8,10)]]
        case('spatial four NOAA wind tiles', tiled)
        case('coordinate per-tile actual ranges', tiled, coordinate_ranges=True)
        case('spatial Float32 coordinate gaps', [([('lat', np.float32(40)+np.arange(8,dtype='float32')*np.float32(.1)+np.float32(i*1.2)), ('lon',lon[:5])],values[i*8:(i+1)*8,:5]) for i in range(2)], coordinate_dtype=gdal.GDT_Float32)
        case('Float32 coordinate Zarr snapshot', [([('lat', np.float32(40)+np.arange(8,dtype='float32')*np.float32(.1)+np.float32(i*1.2)), ('lon',lon[:5])],values[i*8:(i+1)*8,:5]) for i in range(2)], coordinate_dtype=gdal.GDT_Float32, output='zarr')
        case('spatial Float64 source coordinate bits', [([('lat', 40. + np.arange(8)*.1 + i*1.2), ('lon',lon[:5])],values[i*8:(i+1)*8,:5]) for i in range(2)])
        case('CF shared unindexed bounds dimension', tiled, bounds=True)
        case('CF unindexed bounds Zarr snapshot', tiled, bounds=True, output='zarr')
        case('CF selected payload retains bounds', tiled, bounds=True, selected=True)
        case('CF selected payload bounds Zarr snapshot', tiled, bounds=True, selected=True, output='zarr')
        case('CF selected two-dimensional auxiliary coordinates', tiled, auxiliary=True, selected=True)
        case('CF selected auxiliary coordinates Zarr snapshot', tiled, auxiliary=True, selected=True, output='zarr')
        case('descending latitude normalized GDAL reference', [([(n, c[::-1] if n == 'lat' else c) for n,c in axes], np.flip(data, axis=0)) for axes,data in tiled], normalize_descending=True)
        case('spatial missing quadrants', [tiled[0],tiled[3]])
        case('CF missing_value without _FillValue gaps', [tiled[0], tiled[3]], missing_value_only=True)
        case('spatial gaps along both axes', [tiled[0],([('lat',lat[10:]),('lon',lon[12:])],values[10:,12:])])
        overlap = [([('lat',lat[:10]),('lon',lon[:12])], values[:10,:12]),
                   ([('lat',lat[6:]),('lon',lon[8:])], values[6:,8:] + 17)]
        overlap[1][1][:2,:2] = -99
        case('spatial overlap last input includes nodata', overlap)
        case('groups selected nested wind', tiled, nested=True, selected=True)
        case('zarr spatial coordinate snapshot', tiled, output='zarr')
        case('temporal irregular singleton ordering', [([('time',[t]),('lat',lat[:4]),('lon',lon[:5])],values[None,:4,:5]+i) for i,t in enumerate([10,17,12])])
        case('temporal irregular multisample blocks', [([('time',t),('lat',lat[:4]),('lon',lon[:5])], np.stack([values[:4,:5]+i for i in range(len(t))])) for t in [[0,1,3],[4,7]]])
        for label, coordinates in [('duplicate', [0,0,1]), ('unsorted', [0,2,1])]:
            sources = []
            for index in range(2):
                source = tmp/f'{label}-{index}.nc'
                create(source, [('lat',coordinates), ('lon',[0])], [1,2,3])
                sources.append(str(source))
            reference = subprocess.run(['gdal','mdim','mosaic',*sources,str(tmp/f'{label}.vrt')],capture_output=True)
            destination = tmp/f'{label}.nc'
            actual = subprocess.run([args.terrio,'mdmosaic',*sources,str(destination)],capture_output=True)
            results.append({'name':f'identical {label} coordinate refusal','expected_rejection':True,
                            'same_rejection':reference.returncode == 1 and actual.returncode == 1
                            and b'strictly' in reference.stderr and b'strictly monotonic' in actual.stderr and not destination.exists()})
        # GDAL rejects an off-grid singleton mixed with a regular series.
        create(tmp/'regular.nc', [('time',[0,6,12]),('x',[0])], np.array([[1],[2],[3]]))
        create(tmp/'singleton.nc', [('time',[19]),('x',[0])], np.array([[4]]))
        ref = subprocess.run(['gdal','mdim','mosaic',str(tmp/'regular.nc'),str(tmp/'singleton.nc'),str(tmp/'rejected-reference.vrt')], capture_output=True)
        actual = tmp/'rejected-terrio.nc'
        out = subprocess.run([args.terrio,'mdmosaic',str(tmp/'regular.nc'),str(tmp/'singleton.nc'),str(actual)], capture_output=True)
        results.append({'name':'mixed off-grid singleton rejection', 'expected_rejection':True,
                        'same_rejection':ref.returncode == 1 and out.returncode == 1 and b'not aligned' in out.stderr and b'irregularly-spaced values' in ref.stderr.lower() and not actual.exists()})
        # Distant Float32 origins can have measurably different stored spacing.
        # Both implementations reject rather than silently moving those grids.
        for offset in [900, 1000]:
            sources = []
            for index, origin in enumerate([0, offset]):
                source = tmp / f'precision-{offset}-{index}.nc'
                create(source, [('time', np.asarray([40+(origin+j)*.1 for j in range(8)], dtype='float32')), ('x',[0])],
                       np.ones((8,1)), coordinate_dtype=gdal.GDT_Float32)
                sources.append(str(source))
            ref = subprocess.run(['gdal', 'mdim', 'mosaic', *sources, str(tmp/f'precision-{offset}.vrt')], capture_output=True)
            destination = tmp/f'precision-{offset}.nc'
            out = subprocess.run([args.terrio, 'mdmosaic', *sources, str(destination)], capture_output=True)
            results.append({'name':f'Float32 distant-origin precision refusal {offset}', 'expected_rejection':True,
                            'same_rejection':ref.returncode == 1 and out.returncode == 1 and b'spacing' in ref.stderr
                            and b'not aligned' in out.stderr and not destination.exists()})
        for dtype in [gdal.GDT_Byte,gdal.GDT_Int8,gdal.GDT_UInt16,gdal.GDT_Int16,gdal.GDT_UInt32,gdal.GDT_Int32,gdal.GDT_UInt64,gdal.GDT_Int64,gdal.GDT_Float64]:
            if dtype in (gdal.GDT_UInt64,gdal.GDT_Int64):
                # Form 64-bit values in integer arithmetic, without a float64 round trip.
                converted = [(axes, np.abs(v).astype('int64')*3 + 2**53+13) for axes,v in tiled]
            else:
                converted = [(axes, np.abs(v)*3) for axes,v in tiled]
            case('types '+gdal.GetDataTypeName(dtype), converted, dtype)
    print(json.dumps(results, indent=1) if args.json else '\n'.join(str(r) for r in results))
    def failed(result):
        if 'error' in result or (result.get('differing') and result.get('within_tol') != 1):
            return True
        for key, value in result.items():
            if key.startswith('same_') and value is False:
                if key == 'same_type' and result.get('permitted_coordinate_type_difference'):
                    continue
                return True
        return False

    return int(any(failed(result) for result in results))


if __name__ == '__main__':
    raise SystemExit(main())
