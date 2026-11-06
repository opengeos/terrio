//! Sets `cfg(remote)`: the `remote` feature on a target with a network stack (not WebAssembly),
//! where terrio reads URLs and object stores; and `cfg(threads)`: a target with threads, which
//! WebAssembly has when built with its `atomics` feature (a thread pool started by the host, see
//! the `terrio-js` package's threaded module); and `cfg(fs)`: a target with a filesystem, which
//! is every target but WebAssembly in a browser (`wasm32-unknown-unknown`): WASI has one, which
//! the browser build of the command line (`terrio.dev/web`) runs on.

fn main() {
    println!("cargo::rustc-check-cfg=cfg(remote)");
    println!("cargo::rustc-check-cfg=cfg(threads)");
    println!("cargo::rustc-check-cfg=cfg(fs)");
    let wasm = std::env::var("CARGO_CFG_TARGET_FAMILY").is_ok_and(|f| f.split(',').any(|f| f == "wasm"));
    let atomics = std::env::var("CARGO_CFG_TARGET_FEATURE").is_ok_and(|f| f.split(',').any(|f| f == "atomics"));
    if !wasm || atomics {
        println!("cargo::rustc-cfg=threads");
    }
    if !wasm || std::env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "wasi") {
        println!("cargo::rustc-cfg=fs");
    }
    if std::env::var_os("CARGO_FEATURE_REMOTE").is_some() && !wasm {
        println!("cargo::rustc-cfg=remote");
    }
}
