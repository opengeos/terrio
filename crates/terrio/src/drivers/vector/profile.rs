//! What writers learn from a layer's features before writing it: the field widths and value
//! ranges, geometry types, dimensions and extent that decide column types and headers.
//!
//! A writer of a whole layer computes the profile from its features ([`LayerProfile::of`]); a
//! streaming writer receives it from a first pass over the data ([`LayerProfile::add`] on each
//! chunk), so both make the same decisions.

use wbvector::{Feature, FieldType, FieldValue, GeometryType, Layer};

/// What the values of one field hold.
#[derive(Debug, Clone, PartialEq)]
pub struct FieldProfile {
    /// The number of non-null values.
    pub non_null: usize,
    /// Every non-null value is an integer (or a real) in the range of a 32-bit integer.
    pub all_int32: bool,
    /// No value that converts to an integer falls outside the range of a 32-bit integer.
    pub no_int_beyond_int32: bool,
    /// The longest decimal representation of the values that convert to integers.
    pub longest_int: usize,
    /// The longest value as text (non-null values).
    pub longest_text: usize,
    /// Every non-null value is a `YYYY-MM-DD` date.
    pub all_dates: bool,
    /// Every non-null value is an ISO 8601 date-time.
    pub all_datetimes: bool,
    /// The number of non-null values that are date-times with a time zone offset.
    pub with_offset: usize,
}

impl Default for FieldProfile {
    fn default() -> Self {
        Self {
            non_null: 0,
            all_int32: true,
            no_int_beyond_int32: true,
            longest_int: 0,
            longest_text: 0,
            all_dates: true,
            all_datetimes: true,
            with_offset: 0,
        }
    }
}

/// The text of a value, as the Shapefile writer measures it.
pub(crate) fn text_len(v: &FieldValue) -> usize {
    match v {
        FieldValue::Text(s) | FieldValue::Date(s) | FieldValue::DateTime(s) => s.len(),
        FieldValue::Integer(x) => x.to_string().len(),
        FieldValue::Float(x) => x.to_string().len(),
        FieldValue::Boolean(b) => b.to_string().len(),
        FieldValue::Blob(b) => 2 * b.len(),
        FieldValue::Null => 0,
    }
}

/// The number of characters of an integer in decimal.
fn int_len(x: i64) -> usize {
    let digits = if x == 0 { 1 } else { x.unsigned_abs().ilog10() as usize + 1 };
    digits + usize::from(x < 0)
}

impl FieldProfile {
    /// Adds the profile of the values that follow.
    fn merge(&mut self, o: &FieldProfile) {
        self.non_null += o.non_null;
        self.all_int32 &= o.all_int32;
        self.no_int_beyond_int32 &= o.no_int_beyond_int32;
        self.longest_int = self.longest_int.max(o.longest_int);
        self.longest_text = self.longest_text.max(o.longest_text);
        self.all_dates &= o.all_dates;
        self.all_datetimes &= o.all_datetimes;
        self.with_offset += o.with_offset;
    }

    /// Adds a value of a field of type `t`. The text length is measured for the field types
    /// written as text (the others have fixed widths), and dates are parsed for date fields.
    fn add(&mut self, v: &FieldValue, t: FieldType) {
        if let Some(x) = v.as_i64() {
            self.no_int_beyond_int32 &= i32::try_from(x).is_ok();
            self.longest_int = self.longest_int.max(int_len(x));
        }
        if v.is_null() {
            return;
        }
        self.non_null += 1;
        self.all_int32 &= v.as_i64().is_some_and(|x| i32::try_from(x).is_ok());
        if !matches!(t, FieldType::Integer | FieldType::Float | FieldType::Boolean | FieldType::Date) {
            self.longest_text = self.longest_text.max(text_len(v));
        }
        if matches!(t, FieldType::Date | FieldType::DateTime) {
            let s = v.as_str();
            self.all_dates &= s.and_then(super::geoparquet::parse_date).is_some();
            self.all_datetimes &= s.and_then(super::geoparquet::parse_datetime).is_some();
            if s.is_some_and(crate::arrow::has_offset) {
                self.with_offset += 1;
            }
        }
    }
}

/// The type two geometry types share, single and multi-part forms counting as the multi-part
/// type: `None` when they differ.
fn shared_type(a: GeometryType, b: GeometryType) -> Option<GeometryType> {
    use GeometryType as T;
    match (a, b) {
        (a, b) if a == b => Some(a),
        (T::Point | T::MultiPoint, T::Point | T::MultiPoint) => Some(T::MultiPoint),
        (T::LineString | T::MultiLineString, T::LineString | T::MultiLineString) => Some(T::MultiLineString),
        (T::Polygon | T::MultiPolygon, T::Polygon | T::MultiPolygon) => Some(T::MultiPolygon),
        _ => None,
    }
}

/// What the geometries of a layer hold.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GeometryProfile {
    /// The number of features with a geometry.
    pub count: usize,
    /// The type of the first geometry, and whether every geometry has it.
    pub first: Option<(GeometryType, bool)>,
    /// The type the geometries share, single and multi-part forms counting as the multi-part
    /// type (see `features_type`): `None` before any geometry, `Some(None)` for mixed types.
    pub shared: Option<Option<GeometryType>>,
    /// Some geometry has Z coordinates.
    pub has_z: bool,
    /// Some coordinate has a Z value.
    pub any_z: bool,
    /// Some coordinate has an M value.
    pub has_m: bool,
    /// Some geometry is not a point.
    pub non_point: bool,
    /// Some feature has no geometry, or an empty one (no bounding box).
    pub missing_bbox: bool,
    /// The geometry type names (`Polygon`, `Point Z`), in the order they first appear.
    pub type_names: Vec<String>,
    /// The extent `[min_x, min_y, max_x, max_y]`.
    pub bbox: Option<[f64; 4]>,
}

/// What writers learn from a layer's features.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LayerProfile {
    /// The number of features.
    pub count: usize,
    /// Each field's profile, in schema order.
    pub fields: Vec<FieldProfile>,
    /// The geometries' profile.
    pub geometry: GeometryProfile,
    types: Vec<FieldType>,
}

impl LayerProfile {
    /// An empty profile for the features of `layer` (its fields; its features are not read).
    pub fn new(layer: &Layer) -> Self {
        let types: Vec<FieldType> = layer.schema.fields().iter().map(|f| f.field_type).collect();
        Self { count: 0, fields: vec![FieldProfile::default(); types.len()], geometry: GeometryProfile::default(), types }
    }

    /// The profile of a layer's features.
    pub fn of(layer: &Layer) -> Self {
        let mut p = Self::new(layer);
        p.add(&layer.features);
        p
    }

    /// Adds features to the profile (large chunks in parallel, merged in order).
    pub fn add(&mut self, features: &[Feature]) {
        const PART: usize = 4096;
        if features.len() <= PART {
            return self.add_seq(features);
        }
        let parts = crate::par::map_range(features.len().div_ceil(PART), |i| {
            let mut p = Self {
                count: 0,
                fields: vec![FieldProfile::default(); self.types.len()],
                geometry: GeometryProfile::default(),
                types: self.types.clone(),
            };
            p.add_seq(&features[i * PART..((i + 1) * PART).min(features.len())]);
            p
        });
        for p in &parts {
            self.merge(p);
        }
    }

    /// Adds the profile of the features that follow.
    fn merge(&mut self, o: &LayerProfile) {
        self.count += o.count;
        for (a, b) in self.fields.iter_mut().zip(&o.fields) {
            a.merge(b);
        }
        let (g, h) = (&mut self.geometry, &o.geometry);
        g.count += h.count;
        g.first = match (g.first, h.first) {
            (None, f) => f,
            (Some(f), None) => Some(f),
            (Some((t, same)), Some((u, same2))) => Some((t, same && same2 && t == u)),
        };
        g.shared = match (g.shared, h.shared) {
            (None, s) => s,
            (s, None) => s,
            (Some(Some(a)), Some(Some(b))) => Some(shared_type(a, b)),
            _ => Some(None),
        };
        g.has_z |= h.has_z;
        g.any_z |= h.any_z;
        g.has_m |= h.has_m;
        g.non_point |= h.non_point;
        g.missing_bbox |= h.missing_bbox;
        for n in &h.type_names {
            if !g.type_names.contains(n) {
                g.type_names.push(n.clone());
            }
        }
        g.bbox = match (g.bbox, h.bbox) {
            (Some(a), Some(b)) => Some([a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])]),
            (a, b) => a.or(b),
        };
    }

    fn add_seq(&mut self, features: &[Feature]) {
        for f in features {
            self.count += 1;
            for ((fp, v), t) in self.fields.iter_mut().zip(&f.attributes).zip(&self.types) {
                fp.add(v, *t);
            }
            let g = &mut self.geometry;
            let Some(geom) = &f.geometry else {
                g.missing_bbox = true;
                continue;
            };
            g.count += 1;
            let t = geom.geom_type();
            g.first = Some(match g.first {
                None => (t, true),
                Some((first, same)) => (first, same && first == t),
            });
            g.shared = Some(match g.shared {
                None => Some(t),
                Some(None) => None,
                Some(Some(a)) => shared_type(a, t),
            });
            let z = crate::vector::geometry_has_z(geom);
            g.has_z |= z;
            if !(g.any_z && g.has_m) {
                let (az, am) = crate::arrow::geometry_dims(geom);
                g.any_z |= az;
                g.has_m |= am;
            }
            g.non_point |= !matches!(geom, wbvector::Geometry::Point(_));
            let (base, suffix) = (t.as_str(), if z { " Z" } else { "" });
            if !g.type_names.iter().any(|n| n.strip_prefix(base) == Some(suffix)) {
                g.type_names.push(format!("{base}{suffix}"));
            }
            match geom.bbox() {
                Some(b) => {
                    g.bbox = Some(match g.bbox {
                        None => [b.min_x, b.min_y, b.max_x, b.max_y],
                        Some(a) => [a[0].min(b.min_x), a[1].min(b.min_y), a[2].max(b.max_x), a[3].max(b.max_y)],
                    });
                }
                None => g.missing_bbox = true,
            }
        }
    }

    /// The geometry type every feature has (see `uniform_type`).
    pub fn uniform_type(&self) -> Option<GeometryType> {
        self.geometry.first.filter(|(_, same)| *same).map(|(t, _)| t)
    }

    /// The type the geometries share, single and multi-part forms counting as the multi-part
    /// type; `None` when there are no geometries or they mix other types.
    pub fn features_type(&self) -> Option<GeometryType> {
        self.geometry.shared.flatten()
    }

    /// Whether no feature has a geometry.
    pub fn no_geometry(&self) -> bool {
        self.geometry.count == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wbvector::{FieldDef, FieldType, Geometry};

    #[test]
    fn profiles_match_whole_layer_decisions() {
        let mut l = Layer::new("t");
        l.add_field(FieldDef::new("i", FieldType::Integer));
        l.add_field(FieldDef::new("d", FieldType::Date));
        for (i, (v, g)) in [
            (FieldValue::Integer(3_000_000_000), Some(Geometry::point(1.0, 2.0))),
            (FieldValue::Integer(-5), Some(Geometry::multi_point(vec![wbvector::Coord::xy(0.0, 0.0)]))),
            (FieldValue::Null, None),
        ]
        .into_iter()
        .enumerate()
        {
            l.push(Feature { fid: i as u64, geometry: g, attributes: vec![v, FieldValue::Date("2024-01-02".into())] });
        }
        let p = LayerProfile::of(&l);
        assert_eq!(p.count, 3);
        assert_eq!((int_len(0), int_len(-5), int_len(1000), int_len(i64::MIN)), (1, 2, 4, 20));
        assert!(!p.fields[0].all_int32 && !p.fields[0].no_int_beyond_int32);
        assert_eq!(p.fields[0].longest_int, 10);
        assert!(p.fields[1].all_dates);
        assert_eq!(p.features_type(), Some(GeometryType::MultiPoint));
        assert_eq!(p.uniform_type(), super::super::uniform_type(&l));
        assert_eq!(p.geometry.type_names, ["Point", "MultiPoint"]);
        assert!(p.geometry.missing_bbox);
        // Chunks add up to the whole.
        let mut q = LayerProfile::new(&l);
        q.add(&l.features[..1]);
        q.add(&l.features[1..]);
        assert_eq!(p, q);
    }

    /// Large chunks, profiled in parallel parts, give the profile of one sequential pass.
    #[test]
    fn parallel_profiles_match_sequential_ones() {
        let mut l = Layer::new("t");
        l.add_field(FieldDef::new("i", FieldType::Integer));
        l.add_field(FieldDef::new("d", FieldType::Date));
        l.add_field(FieldDef::new("s", FieldType::Text));
        for k in 0..20_000i64 {
            let geometry = match k % 7 {
                0 => None,
                1 | 2 => Some(Geometry::point(k as f64, -(k as f64))),
                3 => Some(Geometry::multi_point(vec![wbvector::Coord::xyz(1.0, 2.0, k as f64)])),
                _ if k > 15_000 => Some(Geometry::line_string(vec![wbvector::Coord::xy(0.0, 0.0), wbvector::Coord::xy(k as f64, 1.0)])),
                _ => Some(Geometry::point(0.5, 0.5)),
            };
            let i = if k == 12_345 { FieldValue::Integer(1 << 40) } else { FieldValue::Integer(k) };
            let d = if k == 19_000 { FieldValue::Date("not a date".into()) } else { FieldValue::Date("2024-01-02".into()) };
            let s = if k % 11 == 0 { FieldValue::Null } else { FieldValue::Text("x".repeat((k % 300) as usize)) };
            l.push(Feature { fid: k as u64, geometry, attributes: vec![i, d, s] });
        }
        let parallel = LayerProfile::of(&l);
        let mut sequential = LayerProfile::new(&l);
        sequential.add_seq(&l.features);
        assert_eq!(parallel, sequential);
        // From a part with no geometry on.
        let mut p = LayerProfile::new(&l);
        p.add(&l.features[..1]);
        p.add(&l.features[1..]);
        assert_eq!(p, sequential);
    }
}
