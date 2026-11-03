//! `terrio preview`: PNG files, PNG bytes and terminal output from rasters and vector datasets.
use std::process::{Command, Output};

fn run(directory: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_terrio")).args(args).current_dir(directory).env_remove("TMUX").output().unwrap()
}

fn grid(directory: &std::path::Path) {
    std::fs::write(
        directory.join("grid.asc"),
        "ncols 4\nnrows 2\nxllcorner 0\nyllcorner 0\ncellsize 1\nNODATA_value -9\n1 2 3 4\n5 6 7 -9\n",
    )
    .unwrap();
}

/// The width, height and RGBA pixels of a PNG.
fn decode(bytes: &[u8]) -> (u32, u32, Vec<u8>) {
    let image = terrio::memory::open_raster_bytes(bytes.to_vec(), "image.png").unwrap();
    let (cols, rows, bands) = (image.raster.cols, image.raster.rows, image.raster.bands);
    assert_eq!(bands, 4);
    let values = image.read_window_f64(terrio::Window::full(cols, rows), &Default::default()).unwrap();
    let n = cols * rows;
    let rgba = (0..n).flat_map(|p| (0..4).map(move |b| (p, b))).map(|(p, b)| values[b * n + p] as u8).collect();
    (cols as u32, rows as u32, rgba)
}

#[test]
fn writes_a_png_with_transparent_nodata_and_refuses_bad_outputs() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    grid(root);
    let made = run(root, &["preview", "grid.asc", "-o", "grid.png", "-c", "viridis", "-s", "minmax"]);
    assert!(made.status.success(), "{}", String::from_utf8_lossy(&made.stderr));
    let (width, height, rgba) = decode(&std::fs::read(root.join("grid.png")).unwrap());
    assert_eq!((width, height), (4, 2));
    assert_eq!(&rgba[..4], &[0x44, 0x01, 0x54, 255]);
    assert_eq!(rgba[7 * 4 + 3], 0, "the nodata pixel is transparent");
    for (args, message) in [
        (vec!["preview", "grid.asc", "-o", "grid.png"], "exists"),
        (vec!["preview", "grid.asc", "-o", "grid.jpg"], ".png"),
        (vec!["preview", "grid.asc", "-o", "x.png", "-c", "nope"], "colormap"),
        (vec!["preview", "grid.asc", "-o", "x.png", "-a", "field"], "vector"),
        (vec!["preview", "grid.asc"], "not a terminal"),
        (vec!["preview", "grid.asc", "-o", "x.png", "--display", "kitty"], "--display"),
        (vec!["preview", "missing.tif", "-o", "x.png"], "neither a raster nor a vector dataset"),
        (vec!["preview", "OAPIF:not-a-url", "-o", "x.png"], "neither a raster nor a vector dataset"),
    ] {
        let output = run(root, &args);
        assert!(!output.status.success(), "{args:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains(message), "{args:?}: {}", String::from_utf8_lossy(&output.stderr));
    }
}

#[test]
fn overwrite_replaces_the_file_and_leaves_nothing_beside_it() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    grid(root);
    std::fs::write(root.join("grid.png"), b"old").unwrap();
    let made = run(root, &["preview", "grid.asc", "-o", "grid.png", "--overwrite", "--width", "8"]);
    assert!(made.status.success(), "{}", String::from_utf8_lossy(&made.stderr));
    assert_eq!(decode(&std::fs::read(root.join("grid.png")).unwrap()).0, 8);
    let mut names: Vec<_> = std::fs::read_dir(root).unwrap().map(|e| e.unwrap().file_name()).collect();
    names.sort();
    assert_eq!(names, ["grid.asc", "grid.png"]);
}

#[test]
fn sends_png_bytes_or_terminal_drawing_to_standard_output() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    grid(root);
    let png = run(root, &["preview", "grid.asc", "-o", "-", "--width", "8"]);
    assert!(png.status.success());
    assert_eq!(decode(&png.stdout).0, 8);
    let blocks = run(root, &["preview", "grid.asc", "--display", "blocks"]);
    assert!(blocks.status.success());
    let text = String::from_utf8(blocks.stdout).unwrap();
    assert_eq!(text.lines().count(), 1);
    assert!(text.contains('▀'));
    let kitty = run(root, &["preview", "grid.asc", "--display", "kitty"]);
    assert!(kitty.stdout.starts_with(b"\x1b_Gf=100,a=T"));
}

#[test]
fn draws_vector_features_and_their_attributes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let square = |x: f64, value: f64| {
        format!(
            r#"{{"type":"Feature","properties":{{"v":{value}}},"geometry":{{"type":"Polygon","coordinates":[[[{x},0],[{},0],[{},1],[{x},1],[{x},0]]]}}}}"#,
            x + 1.0,
            x + 1.0
        )
    };
    std::fs::write(
        root.join("squares.geojson"),
        format!(r#"{{"type":"FeatureCollection","features":[{},{}]}}"#, square(0.0, 1.0), square(2.0, 2.0)),
    )
    .unwrap();
    let made = run(root, &["preview", "squares.geojson", "-o", "squares.png", "--max-size", "30", "-a", "v", "-c", "gray", "-s", "minmax"]);
    assert!(made.status.success(), "{}", String::from_utf8_lossy(&made.stderr));
    let (width, height, rgba) = decode(&std::fs::read(root.join("squares.png")).unwrap());
    assert_eq!((width, height), (30, 10));
    let at = |col: usize, row: usize| &rgba[(row * 30 + col) * 4..(row * 30 + col) * 4 + 4];
    assert_eq!(at(5, 5), &[0, 0, 0, 255]);
    assert_eq!(at(25, 5), &[255, 255, 255, 255]);
    assert_eq!(at(15, 5)[3], 0);
}
