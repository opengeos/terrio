fn main() {
    println!("cargo::rustc-check-cfg=cfg(remote)");
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-changed=data/epsg.tsv.zst");
    let wasm = std::env::var("CARGO_CFG_TARGET_FAMILY").is_ok_and(|f| f.split(',').any(|f| f == "wasm"));
    if std::env::var_os("CARGO_FEATURE_GRID_TRANSPORT").is_some() && !wasm {
        println!("cargo::rustc-cfg=remote");
    }
    // Native builds embed the EPSG table decompressed (src/epsg.rs, `epsg_text`); WebAssembly
    // modules keep it compressed.
    if !wasm {
        let text = zrip::decompress(include_bytes!("data/epsg.tsv.zst")).expect("data/epsg.tsv.zst is a Zstandard frame");
        assert!(std::str::from_utf8(&text).is_ok(), "data/epsg.tsv.zst holds UTF-8 text");
        let out = std::path::Path::new(&std::env::var_os("OUT_DIR").expect("OUT_DIR is set")).join("epsg.tsv");
        std::fs::write(out, text).expect("the decompressed EPSG table is written to OUT_DIR");
    }
}
