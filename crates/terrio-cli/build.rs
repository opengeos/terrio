//! Turns `benches/cases.txt` into the benchmark function of `benches/commands.rs`, `cli`: gungraun
//! names a case at compile time, by an attribute, so each line becomes one `#[bench::<name>(...)]`.

use std::fmt::Write;
use std::path::Path;

fn main() {
    println!("cargo::rerun-if-changed=benches/cases.txt");
    // Packaged crates may leave the benchmarks out: then there is nothing to build them from.
    let Ok(text) = std::fs::read_to_string("benches/cases.txt") else { return };
    let mut cases = String::from("#[library_benchmark]\n");
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let (name, command) = line.split_once(char::is_whitespace).unwrap_or_else(|| panic!("cases.txt: no command after '{line}'"));
        assert!(
            name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
            "cases.txt: '{name}' is not a case name (lowercase letters, digits and _)"
        );
        writeln!(cases, "#[bench::{name}(args = ({:?}), setup = prepare)]", command.trim()).unwrap();
    }
    cases.push_str("fn cli(case: Case) -> tempfile::TempDir {\n    run(case)\n}\n");
    std::fs::write(Path::new(&std::env::var("OUT_DIR").unwrap()).join("cases.rs"), cases).unwrap();
}
