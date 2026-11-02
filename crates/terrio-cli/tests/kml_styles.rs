//! KML shared styles through `terrio convert`, on the real executable.

use std::process::Command;

fn convert(input: &std::path::Path, output: &std::path::Path) {
    let r =
        Command::new(env!("CARGO_BIN_EXE_terrio")).args(["convert", input.to_str().unwrap(), output.to_str().unwrap()]).output().unwrap();
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
}

#[test]
fn convert_keeps_shared_styles_in_kml_and_resolves_them_elsewhere() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("a.kml");
    std::fs::write(
        &input,
        r##"<kml><Document>
        <Style id="base"><LineStyle><color>ff0000ff</color><width>2</width></LineStyle></Style>
        <Placemark><styleUrl>#base</styleUrl><Point><coordinates>30,-2</coordinates></Point></Placemark>
        </Document></kml>"##,
    )
    .unwrap();

    let kml = dir.path().join("o.kml");
    convert(&input, &kml);
    let text = std::fs::read_to_string(&kml).unwrap();
    assert!(text.contains(r#"<Style id="base">"#) && text.contains("<styleUrl>#base</styleUrl>"), "{text}");

    let geojson = dir.path().join("o.geojson");
    convert(&input, &geojson);
    let text = std::fs::read_to_string(&geojson).unwrap();
    assert!(text.contains("PEN(c:#FF0000FF,w:2.000000px)") && !text.contains("@base"), "{text}");
}
