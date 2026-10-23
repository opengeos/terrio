//! The product definition templates of GRIB2 section 4, as g2clib (GDAL's copy, `pdstemplates.c`)
//! describes them: the octet width of each value (negative for signed values, stored as sign and
//! magnitude), for GDAL's `GRIB_PDS_TEMPLATE_ASSEMBLED_VALUES`.

/// The templates: number, whether the template is extended by its values, and the widths.
const TEMPLATES: &[(u16, bool, &[i8])] = &[
    (0, false, &[1, 1, 1, 1, 1, 2, 1, 1, -4, 1, -1, -4, 1, -1, -4]),
    (1, false, &[1, 1, 1, 1, 1, 2, 1, 1, -4, 1, -1, -4, 1, -1, -4, 1, 1, 1]),
    (2, false, &[1, 1, 1, 1, 1, 2, 1, 1, -4, 1, -1, -4, 1, -1, -4, 1, 1]),
    (3, true, &[1, 1, 1, 1, 1, 2, 1, 1, -4, 1, -1, -4, 1, -1, -4, 1, 1, 1, 1, 1, 1, 1, -4, -4, 4, 4, 1, -1, 4, -1, 4]),
    (4, true, &[1, 1, 1, 1, 1, 2, 1, 1, -4, 1, -1, -4, 1, -1, -4, 1, 1, 1, 1, 1, 1, 1, -4, 4, 4, 1, -1, 4, -1, 4]),
    (5, false, &[1, 1, 1, 1, 1, 2, 1, 1, -4, 1, -1, -4, 1, -1, -4, 1, 1, 1, -1, -4, -1, -4]),
    (6, false, &[1, 1, 1, 1, 1, 2, 1, 1, -4, 1, -1, -4, 1, -1, -4, 1]),
    (7, false, &[1, 1, 1, 1, 1, 2, 1, 1, -4, 1, -1, -4, 1, -1, -4]),
    (8, true, &[1, 1, 1, 1, 1, 2, 1, 1, -4, 1, -1, -4, 1, -1, -4, 2, 1, 1, 1, 1, 1, 1, 4, 1, 1, 1, 4, 1, 4]),
    (9, true, &[1, 1, 1, 1, 1, 2, 1, 1, -4, 1, -1, -4, 1, -1, -4, 1, 1, 1, -1, -4, -1, -4, 2, 1, 1, 1, 1, 1, 1, 4, 1, 1, 1, 4, 1, 4]),
    (10, true, &[1, 1, 1, 1, 1, 2, 1, 1, -4, 1, -1, -4, 1, -1, -4, 1, 2, 1, 1, 1, 1, 1, 1, 4, 1, 1, 1, 4, 1, 4]),
    (11, true, &[1, 1, 1, 1, 1, 2, 1, 1, -4, 1, -1, -4, 1, -1, -4, 1, 1, 1, 2, 1, 1, 1, 1, 1, 1, 4, 1, 1, 1, 4, 1, 4]),
    (12, true, &[1, 1, 1, 1, 1, 2, 1, 1, -4, 1, -1, -4, 1, -1, -4, 1, 1, 2, 1, 1, 1, 1, 1, 1, 4, 1, 1, 1, 4, 1, 4]),
    (
        13,
        true,
        &[
            1, 1, 1, 1, 1, 2, 1, 1, -4, 1, -1, -4, 1, -1, -4, 1, 1, 1, 1, 1, 1, 1, -4, -4, 4, 4, 1, -1, 4, -1, 4, 2, 1, 1, 1, 1, 1, 1, 4,
            1, 1, 1, 4, 1, 4,
        ],
    ),
    (
        14,
        true,
        &[
            1, 1, 1, 1, 1, 2, 1, 1, -4, 1, -1, -4, 1, -1, -4, 1, 1, 1, 1, 1, 1, 1, -4, 4, 4, 1, -1, 4, -1, 4, 2, 1, 1, 1, 1, 1, 1, 4, 1,
            1, 1, 4, 1, 4,
        ],
    ),
    (15, false, &[1, 1, 1, 1, 1, 2, 1, 1, -4, 1, -1, -4, 1, -1, -4, 1, 1, 1]),
    (20, false, &[1, 1, 1, 1, 1, -4, 4, 2, 4, 2, 1, 1, 1, 1, 1, 2, 1, 3, 2]),
    (30, true, &[1, 1, 1, 1, 1]),
    (31, true, &[1, 1, 1, 1, 1]),
    (40, false, &[1, 1, 2, 1, 1, 1, 2, 1, 1, -4, 1, -1, -4, 1, -1, -4]),
    (41, false, &[1, 1, 2, 1, 1, 1, 2, 1, 1, -4, 1, -1, -4, 1, -1, -4, 1, 1, 1]),
    (42, true, &[1, 1, 2, 1, 1, 1, 2, 1, 1, -4, 1, -1, -4, 1, -1, -4, 2, 1, 1, 1, 1, 1, 1, 4, 1, 1, 1, 4, 1, 4]),
    (43, true, &[1, 1, 2, 1, 1, 1, 2, 1, 1, -4, 1, -1, -4, 1, -1, -4, 1, 1, 1, 2, 1, 1, 1, 1, 1, 1, 4, 1, 1, 1, 4, 1, 4]),
    (254, false, &[1, 1, 4]),
    (1000, false, &[1, 1, 1, 1, 1, 2, 1, 1, -4]),
    (1001, false, &[1, 1, 1, 1, 1, 2, 1, 1, 4, 4, 1, 1, 1, 4, 1, 4]),
    (1002, false, &[1, 1, 1, 1, 1, 2, 1, 1, -4, 1, 1, 1, 4, 4, 2]),
    (1100, false, &[1, 1, 1, 1, 1, 2, 1, 1, -4, 1, -1, -4, 1, -1, -4]),
    (1101, false, &[1, 1, 1, 1, 1, 2, 1, 1, -4, 1, -1, -4, 1, -1, -4, 4, 1, 1, 1, 4, 1, 4]),
    (32, true, &[1, 1, 1, 1, 1, 2, 1, 1, -4, 1]),
    (44, false, &[1, 1, 2, 1, -1, -4, -1, -4, 1, 1, 1, 2, 1, 1, -2, 1, -1, -4, 1, -1, -4]),
    (45, false, &[1, 1, 2, 1, -1, -4, -1, -4, 1, 1, 1, 2, 1, 1, -4, 1, -1, -4, 1, -1, -4, 1, 1, 1]),
    (
        46,
        true,
        &[1, 1, 2, 1, -1, -4, -1, -4, 1, 1, 1, 2, 1, 1, -4, 1, -1, -4, 1, -1, -4, 2, 1, 1, 1, 1, 1, 1, 4, 1, 1, 1, 4, 1, 4],
    ),
    (
        47,
        true,
        &[1, 1, 1, 2, 1, -1, -4, -1, -4, 1, 1, 2, 1, 1, -4, 1, -1, -4, 1, -1, -4, 1, 1, 1, 2, 1, 1, 1, 1, 1, 1, 4, 1, 1, 1, 4, 1, 4],
    ),
    (48, false, &[1, 1, 2, 1, -1, -4, -1, -4, 1, -1, -4, -1, -4, 1, 1, 1, 2, 1, 1, -4, 1, -1, -4, 1, -1, -4]),
    (50, false, &[1, 1, 1, 1, 1, 2, 1, 1, -4, 1, -1, -4, 1, -1, -4, 1, 1, 4, 4, 4, 4]),
    (52, false, &[1, 1, 1, 1, 1, 1, 1, 1, 2, 1, 1, -4, 1, -1, -4]),
    (51, true, &[1, 1, 1, 1, 1, 2, 1, 1, -4, 1, -1, -4, 1, -1, -4, 1]),
    (91, true, &[1, 1, 1, 1, 1, 2, 1, 1, -4, 1, -1, -4, 1, -1, -4, 1, 1, 1, -1, -4, -1, -4, 2, 1, 1, 1, 1, 1, 1, 4, 1, 1, 1, 4, 1, 4]),
    (33, true, &[1, 1, 1, 1, 1, 2, 1, 1, -4, 1, 2, 2, 2, -1, -4, 1, 1, 1]),
    (34, true, &[1, 1, 1, 1, 1, 2, 1, 1, -4, 1, 2, 2, 2, -1, -4, 1, 1, 1, 2, 1, 1, 1, 1, 1, 1, 4, 1, 1, 1, 4, 1, 4]),
    (53, true, &[1, 1, 1, 1, 4, 2, 1, 1, 1, 2, 1, 1, -4, 1, -1, -4, 1, -1, -4]),
    (54, true, &[1, 1, 1, 1, 4, 2, 1, 1, 1, 2, 1, 1, -4, 1, -1, -4, 1, -1, -4, 1, 1, 1]),
    (57, true, &[1, 1, 2, 2, 2, 2, 1]),
    (60, false, &[1, 1, 1, 1, 1, 2, 1, 1, -4, 1, -1, -4, 1, -1, -4, 1, 1, 1, 2, 1, 1, 1, 1, 1]),
    (
        61,
        true,
        &[
            1, 1, 1, 1, 1, 2, 1, 1, -4, 1, -1, -4, 1, -1, -4, 1, 1, 1, 2, 1, 1, 1, 1, 1, 2, 1, 1, 1, 1, 1, 1, 4, 1, 1, 1, 4, 1, 4,
        ],
    ),
    (35, true, &[1, 1, 1, 1, 1, 1]),
];


/// Reads a value of `width` octets (negative: sign and magnitude) at `*pos` of `body`, as g2clib's
/// `gbit2` does (bits past the end read as zeros), and advances `*pos`.
fn value(body: &[u8], pos: &mut usize, width: i8) -> i64 {
    let n = width.unsigned_abs() as usize;
    let mut v: u64 = 0;
    for k in 0..n {
        v = (v << 8) | u64::from(body.get(*pos + k).copied().unwrap_or(0));
    }
    *pos += n;
    if width < 0 {
        let top = 1u64 << (8 * n - 1);
        let mag = (v & (top - 1)) as i64;
        if v & top != 0 { -mag } else { mag }
    } else {
        // g2clib's values are 32-bit ints.
        (v as u32) as i32 as i64
    }
}

/// The template's extension (g2clib's `extpdstemplate`): more widths, from its values `list`.
fn extension(number: u16, map: &[i8], list: &[i64]) -> Vec<i8> {
    let at = |i: usize| list.get(i).copied().unwrap_or(0);
    let repeat = |count: i64, from: usize| -> Vec<i8> {
        if count > 1 { (2..=count).flat_map(|_| map[from..from + 6].iter().copied()).collect() } else { Vec::new() }
    };
    let ones = |n: i64| vec![1i8; n.max(0) as usize];
    match number {
        3 => ones(at(26)),
        4 => ones(at(25)),
        8 => repeat(at(21), 23),
        9 => repeat(at(28), 30),
        10 => repeat(at(22), 24),
        11 => repeat(at(24), 26),
        12 => repeat(at(23), 25),
        13 => [repeat(at(37), 39), ones(at(26))].concat(),
        14 => [repeat(at(36), 38), ones(at(25))].concat(),
        30 => (0..at(4).max(0)).flat_map(|_| [2, 2, 1, 1, 4]).collect(),
        31 => (0..at(4).max(0)).flat_map(|_| [2, 2, 2, 1, 4]).collect(),
        42 => repeat(at(22), 24),
        43 => repeat(at(25), 27),
        32 => (0..at(9).max(0)).flat_map(|_| [2, 2, 2, -1, -4]).collect(),
        46 => repeat(at(27), 29),
        47 => repeat(at(30), 32),
        51 => (0..at(15).max(0)).flat_map(|_| [1, 1, -1, -4, -1, -4]).collect(),
        33 => ones(at(9)),
        34 => [repeat(at(24), 26), ones(at(9))].concat(),
        53 | 54 => ones(at(3)),
        91 => [repeat(at(28), 30), ones(at(15))].concat(),
        57 => (0..at(6).max(0)).flat_map(|_| [1, -4, 1, 1, 1, 2, 1, 1, -4, 1, -1, -4, 1, -1, -4]).collect(),
        61 => repeat(at(30), 32),
        _ => Vec::new(),
    }
}

/// GDAL's `GRIB_PDS_TEMPLATE_ASSEMBLED_VALUES`: the values of a template (`body`, the octets from
/// octet 10 of section 4, without its coordinate values), when g2clib knows the template and its
/// octets add up to `body`'s length.
pub(super) fn assembled_values(number: u16, body: &[u8]) -> Option<String> {
    let &(_, needext, map) = TEMPLATES.iter().find(|t| t.0 == number)?;
    // Extensions longer than the section cannot match: no need to build them.
    let mut pos = 0;
    let mut list: Vec<i64> = map.iter().map(|w| value(body, &mut pos, *w)).collect();
    let ext = if needext { extension(number, map, &list) } else { Vec::new() };
    let total: usize = map.iter().chain(ext.iter()).map(|w| w.unsigned_abs() as usize).sum();
    if total != body.len() {
        return None;
    }
    list.extend(ext.iter().map(|w| value(body, &mut pos, *w)));
    let widths = map.iter().chain(ext.iter());
    let texts: Vec<String> = list
        .iter()
        .zip(widths)
        .map(|(v, w)| if *w == 4 { format!("{}", *v as i32 as u32) } else { format!("{}", *v as i32) })
        .collect();
    Some(texts.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assembles_template_4_0() {
        // HRRR's composite reflectivity (GDAL: 16 196 2 0 83 0 0 1 0 10 0 0 255 0 0).
        let body = [16u8, 196, 2, 0, 83, 0, 0, 0, 1, 0, 0, 0, 0, 10, 0, 0, 0, 0, 0, 255, 0, 0, 0, 0, 0];
        assert_eq!(assembled_values(0, &body).as_deref(), Some("16 196 2 0 83 0 0 1 0 10 0 0 255 0 0"));
        assert_eq!(assembled_values(0, &body[..24]), None);
        assert_eq!(assembled_values(65535, &body), None);
    }
}
