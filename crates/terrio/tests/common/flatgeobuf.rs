// SPDX-License-Identifier: MIT
use flatbuffers::FlatBufferBuilder;
use flatgeobuf::{Feature as FgbFeature, FeatureArgs, Geometry as FgbGeometry, GeometryArgs, GeometryType as FgbType, Header, HeaderArgs};
const MAGIC: [u8; 8] = [b'f', b'g', b'b', 3, b'f', b'g', b'b', 0];

pub fn fixture(count: u64, measured: bool, ms: &[f64]) -> Vec<u8> {
    let mut b = FlatBufferBuilder::new();
    let h = Header::create(
        &mut b,
        &HeaderArgs { features_count: count, index_node_size: 0, has_m: measured, geometry_type: FgbType::Point, ..Default::default() },
    );
    b.finish_size_prefixed(h, None);
    let mut bytes = MAGIC.to_vec();
    bytes.extend_from_slice(b.finished_data());
    if !ms.is_empty() {
        let mut b = FlatBufferBuilder::new();
        let xy = b.create_vector(&[1.0, 2.0]);
        let m = b.create_vector(ms);
        let g = FgbGeometry::create(&mut b, &GeometryArgs { xy: Some(xy), m: Some(m), ..Default::default() });
        let f = FgbFeature::create(&mut b, &FeatureArgs { geometry: Some(g), ..Default::default() });
        b.finish_size_prefixed(f, None);
        bytes.extend_from_slice(b.finished_data());
    }
    bytes
}
