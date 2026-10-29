//! The rows a raster already on its zoom level's pixel grid is read into, a strip at a time, are
//! sized from its header: allocated fallibly (see `super::super::pyramid::memory_tests`).

use super::*;
use crate::tiles::pyramid::BaseRows;
use crate::tiles::pyramid::memory_tests::{TOO_WIDE, cannot_allocate, out_of_memory};

#[test]
fn aligned_rows_too_large_to_allocate_are_errors() {
    let cfg = wbraster::RasterConfig {
        cols: 2,
        rows: 2,
        bands: 3,
        data_type: DataType::U8,
        cell_size: 1.0,
        crs: SpatialRef::from_epsg(3857).unwrap().to_crs_info(),
        ..Default::default()
    };
    let data = RasterData::from_f64_vec(DataType::U8, vec![9.0; 12]);
    let mut src = crate::LazyRaster::from(RasterDataset::new(wbraster::Raster::from_data_native(cfg, data).unwrap(), false));
    for src_alpha in [false, true] {
        let mut rows = AlignedRows { src: &mut src, w: TOO_WIDE, dx: 0, dy: 0, data_bands: 3, src_alpha, next: 0 };
        cannot_allocate(&out_of_memory(rows.next_rows(1)));
    }
    // Rows that can be allocated: the source's pixels, opaque, and zeros outside it, opaque too
    // (a source without alpha).
    let mut rows = AlignedRows { src: &mut src, w: 3, dx: 0, dy: 0, data_bands: 3, src_alpha: false, next: 0 };
    let read = rows.next_rows(1).unwrap();
    assert_eq!(read, [vec![9, 9, 0, 9, 9, 0, 9, 9, 0, 255, 255, 255]]);
}
