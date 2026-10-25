fn main() {
    println!("cargo::rustc-check-cfg=cfg(remote)");
    let wasm = std::env::var("CARGO_CFG_TARGET_FAMILY").is_ok_and(|f| f.split(',').any(|f| f == "wasm"));
    if std::env::var_os("CARGO_FEATURE_GRID_TRANSPORT").is_some() && !wasm {
        println!("cargo::rustc-cfg=remote");
    }
}
