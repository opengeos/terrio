//! Sets `cfg(remote)`: the `remote` feature on a target with a network stack (not WebAssembly),
//! where terrio reads URLs and object stores; and `cfg(threads)`: a target with threads, which
//! WebAssembly has when built with its `atomics` feature (a thread pool started by the host, see
//! the `terrio-js` package's threaded module).

fn main() {
    println!("cargo::rustc-check-cfg=cfg(remote)");
    println!("cargo::rustc-check-cfg=cfg(threads)");
    let wasm = std::env::var("CARGO_CFG_TARGET_ARCH").is_ok_and(|arch| arch == "wasm32");
    let atomics = std::env::var("CARGO_CFG_TARGET_FEATURE").is_ok_and(|f| f.split(',').any(|f| f == "atomics"));
    if !wasm || atomics {
        println!("cargo::rustc-cfg=threads");
    }
    if std::env::var_os("CARGO_FEATURE_REMOTE").is_some() && !wasm {
        println!("cargo::rustc-cfg=remote");
    }
}
