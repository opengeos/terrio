//! The process-wide raster allow-list and the formats terrio recognises by their content or by a
//! header beside them, in a binary of its own because a list once set cannot be lifted: under a
//! tile server's list of GTiff, COG, JPEG, JP2OpenJPEG and PNG (and HFA), a Surfer 6 grid named
//! `x.tif` or `x.png`, and ENVI and EHdr data named `.img` with their header beside them, were
//! opened with drivers the list leaves out. Each is refused, on disk and as bytes, and a GeoTIFF
//! still opens. Format identification and the content check see the same: the grid is
//! not identified under any name, its check refuses it, and the GeoTIFF is identified and checked.

use terrio::formats::{Kind, set_allowed_drivers};

/// A 10 x 10 Surfer 6 binary grid (`DSBB`) of the values 0 to 99.
fn surfer_grid() -> Vec<u8> {
    let mut b = b"DSBB".to_vec();
    b.extend_from_slice(&10i16.to_le_bytes());
    b.extend_from_slice(&10i16.to_le_bytes());
    for v in [0.0f64, 9.0, 0.0, 9.0, 0.0, 99.0] {
        b.extend_from_slice(&v.to_le_bytes());
    }
    for v in 0..100u8 {
        b.extend_from_slice(&f32::from(v).to_le_bytes());
    }
    b
}

fn refused<T>(result: terrio::Result<T>) -> String {
    match result {
        Ok(_) => panic!("opened"),
        Err(e) => e.to_string(),
    }
}

#[test]
fn formats_found_by_content_or_a_header_are_refused_without_their_driver() {
    let dir = tempfile::tempdir().unwrap();
    let at = |name: &str, bytes: &[u8]| {
        let path = dir.path().join(name);
        std::fs::write(&path, bytes).unwrap();
        path.to_str().unwrap().to_string()
    };
    let (tif, png, grd) = (at("x.tif", &surfer_grid()), at("x.png", &surfer_grid()), at("x.grd", &surfer_grid()));
    let envi = at("e.img", &(0..100u8).collect::<Vec<u8>>());
    at("e.hdr", b"ENVI\nsamples = 10\nlines = 10\nbands = 1\nheader offset = 0\ndata type = 1\ninterleave = bsq\nbyte order = 0\n");
    let ehdr = at("h.img", &(0..100u8).collect::<Vec<u8>>());
    at("h.hdr", b"ncols 10\nnrows 10\nnbands 1\nnbits 8\nbyteorder I\nlayout bil\n");
    let geotiff = dir.path().join("g.tif");
    let cfg = wbraster::RasterConfig {
        cols: 4,
        rows: 3,
        bands: 1,
        x_min: 10.0,
        y_min: 47.0,
        cell_size: 0.5,
        data_type: wbraster::DataType::F64,
        ..Default::default()
    };
    let raster = wbraster::Raster::from_data_native(cfg, wbraster::raster::RasterData::F64(vec![1.0; 12])).unwrap();
    terrio::write_raster(&raster, geotiff.to_str().unwrap(), &Default::default()).unwrap();
    // Without a list, each opens with the driver its content or its header shows.
    for (path, driver) in [(&tif, "GSBG"), (&png, "GSBG"), (&envi, "ENVI"), (&ehdr, "EHdr")] {
        assert_eq!(terrio::LazyRaster::open(path).unwrap().driver().name(), driver, "{path}");
    }

    set_allowed_drivers(Some(&["GTiff", "COG", "JPEG", "JP2OpenJPEG", "PNG", "HFA"]), Some(Kind::Raster)).unwrap();
    for (path, driver) in [(&tif, "GSBG"), (&png, "GSBG"), (&grd, "GSBG"), (&envi, "ENVI"), (&ehdr, "EHdr")] {
        for message in [refused(terrio::LazyRaster::open(path)), refused(terrio::open_raster(path))] {
            assert!(
                message.contains(&format!("the {driver} driver is not allowed (allowed: GTIFF, COG, JPEG, JP2, PNG, HFA)")),
                "{message}"
            );
        }
    }
    let bytes = refused(terrio::memory::open_raster_bytes(surfer_grid(), "x.png"));
    assert!(bytes.contains("holds GSBG data, and the GSBG driver is not allowed"), "{bytes}");
    assert_eq!(terrio::open_raster(geotiff.to_str().unwrap()).unwrap().driver.name(), "GTiff");

    for path in [&tif, &png, &grd] {
        assert!(terrio::inspection::identify(path, false).unwrap().is_none(), "{path}");
        let message = refused(terrio::check::check_path(path, &Default::default()));
        assert!(message.contains("the GSBG driver is not allowed"), "{message}");
    }
    assert!(terrio::inspection::identify_bytes(&surfer_grid(), "x.png").is_none());
    let message = refused(terrio::check::check_bytes(surfer_grid(), "x.png", &Default::default()));
    assert!(message.contains("the GSBG driver is not allowed"), "{message}");
    let geotiff = geotiff.to_str().unwrap();
    assert_eq!(terrio::inspection::identify(geotiff, false).unwrap().unwrap().driver, "GTiff");
    assert_eq!(terrio::check::check_path(geotiff, &Default::default()).unwrap().raster_samples, 12);
}
