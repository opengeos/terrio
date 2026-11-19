use terrio_crs::{SpatialRef, Transformer};
const D: &str = "/tmp/claude-1000/-mnt-nvme-craft/08310933-b3f6-4ded-b9ee-a01fc150053d/scratchpad/1c/";
#[test]
fn probe() {
    unsafe { std::env::set_var("PROJ_NETWORK", "OFF") };
    let w19 = std::fs::read_to_string(format!("{D}w19.txt")).unwrap();
    let s = SpatialRef::from_wkt(&w19);
    let t = Transformer::new(&SpatialRef::from_epsg(4326).unwrap(), &s).unwrap();
    println!("19 {:?} {:?}", s.epsg, t.transform(34.34, 31.49));
    let w47 = std::fs::read_to_string(format!("{D}w47.txt")).unwrap();
    let s = SpatialRef::from_wkt(&w47);
    let t = Transformer::new(&SpatialRef::from_epsg(4979).unwrap(), &s).unwrap();
    println!("47 {:?} {:?}", s.epsg, t.transform_3d(-2.0, 54.0, 100.0));
}
