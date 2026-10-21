//! Sets `cfg(remote)`: the `remote` feature on a target with a network stack (not WebAssembly),
//! where terrio reads URLs and object stores.

fn main() {
    println!("cargo::rustc-check-cfg=cfg(remote)");
    let wasm = std::env::var("CARGO_CFG_TARGET_ARCH").is_ok_and(|arch| arch == "wasm32");
    if std::env::var_os("CARGO_FEATURE_REMOTE").is_some() && !wasm {
        println!("cargo::rustc-cfg=remote");
    }
}
