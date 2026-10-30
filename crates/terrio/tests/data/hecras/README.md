# HEC-RAS fixtures

Synthetic HEC-RAS-shaped HDF5 files for the HECRAS driver's tests. They contain no model data.
Five are copied unchanged from the engine repository,
[gpt-cmdr/ras-commander-fast-hdf](https://github.com/gpt-cmdr/ras-commander-fast-hdf) (MIT,
Copyright (c) 2024 CLB Engineering Corporation; see NOTICE):

- the `coverage_*` files from `tests/fixtures/coverage/` at
  `d6d52f41cb0ef0d4170f60db25d28e01dd543ecd` (release 0.2.0), whose generator and full manifest
  are listed in that directory's `coverage_manifest.json`;
- `pipes.h5` and `terrain.h5` from `tests/fixtures/terrio_io/` at
  `794b8f949f2179f0f9fcdb51417e3c97778c2358`, written by `make_fixtures.py` there and listed in
  its `manifest.json`.

| File | SHA-256 | Contents |
|---|---|---|
| `coverage_mesh.h5` | `ff8172ccd3ff76e643416faa474110e6ba8bf8ff218333e8423dbe8591491117` | 19 meshes: `Good` (two drawable cells and a one-face cell), precision and width variants, and broken topologies |
| `coverage_mesh_nogroups.h5` | `4ed5ad7c89e14ff367a4a0ac34d2ea85c5b78913d651d62a49fe1eddedef84fd` | Legacy names-only layout (the engine refuses it) |
| `coverage_mesh_nogeometry.h5` | `3117dbec3e3c323ffbec8ea95728a55b3e592d5f792ab6fa15151902e0220485` | No `Geometry/2D Flow Areas` group |
| `results_mesh_xs.h5` | `797d88f9cccaa655ff243b19319bdb7a1cf9e584cce5b3658500066aa96ad429` | Written by `scripts/make_hecras_fixture.py` from `coverage_mesh.h5`: the `Good` mesh with made-up static arrays, summary, time series and three cross sections with summary and unsteady results, two boundary-condition lines with series, two reference points and one reference line with results, one river junction, the cross sections' station-elevation, Manning's n and ineffective-area tables and the mesh's face-point perimeter flags and elevations, in the stored HEC-RAS 6.x/7.0 layout. No model data |
| `results_two_areas.h5` | `c169d352a2439ab7f24c59fa98a6211dbec88d63801a4e55c7629f3416cb0611` | Written by `scripts/make_hecras_area_fixture.py` from `results_mesh_xs.h5`: a second 2D flow area, `Dry`, copied from `Good` without its boundary-condition series, as HEC-RAS stores an area no boundary-condition line feeds. No model data |
| `pipes.h5` | `0df382abd29f03f88ea5423349318c80adf347167be660175ac2fb3610a10f3f` | Two pipe conduits and three pipe nodes in the HEC-RAS 7.0.1 layout, root `File Type` `HEC-RAS Geometry` |
| `terrain.h5` | `47f92cb1eefe16c0818a3f0b9da1cde9de4dee7299367ce5a91a518fdbaeeeac` | An HDF5 file whose root `File Type` is `HEC Terrain`, which the driver must not claim |
| `large_projection.h5` | `45373346997ae0ad2a16955fdc6c2c617fe3f5f5e21df5b39e20f2be17ef40dd` | Written by `scripts/make_hecras_root_fixtures.py` from `pipes.h5`: a 16,028-byte root `Projection` (a well-formed WKT with a padded CRS name), for the metadata byte budget |
| `large_root_attribute.h5` | `53e1fd2f76f16ce9ac8873e87f4fb5cee98a8cf4e1b4bc08f3bd9549ccca1a3e` | Written by the same script from `pipes.h5`: a short `Projection` and an unrelated 16,384-byte root attribute, `Notes` |

Real HEC-RAS models are not committed. `tests/hecras_corpus.rs` reads the files of a corpus
directory (`TERRIO_HECRAS_TEST_DATA`) and compares them with the JSON that
`scripts/hecras_oracle.py` writes with h5py; `scripts/compare_hecras_with_h5py.py` runs both on
the HEC-RAS 7.0.1 files of a directory.
