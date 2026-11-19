// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//! The results of every function that fits or identifies a code, over the #1066 corpus (each
//! EPSG and ESRI CRS's WKT 2, GDAL WKT 1 and ESRI WKT 1, as PROJ 9.9.0 writes them), recorded in
//! `tests/data/fits_baseline.tsv` before #1058 step 4 moved them onto one comparison: the move
//! changes none. Temporary: removed with the first fix of step 4.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::{Def, SpatialRef, epsg};

use crate::model::oracle_tests::projjson_compare::{check_recorded, corpus};

/// One file per build: wbprojection's registry identifies more with `wbprojection-fallback`.
#[cfg(not(feature = "wbprojection-fallback"))]
const BASELINE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/fits_baseline.tsv");
#[cfg(feature = "wbprojection-fallback")]
const BASELINE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/fits_baseline_wbprojection.tsv");

/// The codes near a CRS's: the first geographic CRSs on its datum and the first projected CRSs on
/// its geographic CRS, its own left out.
fn neighbours(code: u32) -> Vec<u32> {
    let Some(e) = epsg::get(code) else { return Vec::new() };
    let geographic = epsg::geographic_on(e.datum).iter().copied().take(3);
    let projected = epsg::projected_on(e.geog).iter().map(|(c, _)| *c).take(3);
    geographic.chain(projected).filter(|c| *c != code).collect()
}

/// The results for one text of the corpus. A code is given relative to the text's own where that
/// is exact (`=` for it, `g` for its registered geographic CRS), so that CRSs alike group
/// together; else as itself.
fn outcomes(code: &str, text: &str) -> String {
    let own_epsg = code.strip_prefix("EPSG:").and_then(|c| c.parse::<u32>().ok());
    let own_esri = code.strip_prefix("ESRI:").and_then(|c| c.parse::<u32>().ok());
    let own = own_epsg.or(own_esri);
    let entry = own_epsg.and_then(epsg::get).or_else(|| own_esri.and_then(epsg::esri));
    let rel = |c: Option<u32>| match c {
        None => "-".to_string(),
        Some(c) if Some(c) == own => "=".to_string(),
        Some(c) if entry.is_some_and(|e| e.geog == c) => "g".to_string(),
        Some(c) => c.to_string(),
    };
    let mut out = Vec::new();
    if let Some(c) = own_epsg {
        out.push(format!("fits:{}", u8::from(crate::declared_fits(text, c))));
        out.push(format!("meridian:{}", u8::from(crate::declared_fits_but_meridian(text, c))));
        // Which of them fit, by place (`1` fits, `0` not).
        let near: String = neighbours(c).into_iter().map(|n| if crate::declared_fits(text, n) { '1' } else { '0' }).collect();
        out.push(format!("near:{near}"));
    }
    if let Some(c) = own_esri {
        out.push(format!("esri_fits:{}", u8::from(crate::esri_fits(text, c))));
    }
    out.push(format!("identify:{}", rel(crate::identify_epsg(text))));
    out.push(format!("esri:{}", rel(crate::identify_esri(text))));
    let s = SpatialRef::from_wkt(text);
    out.push(format!("from_wkt:{}/{}", rel(s.epsg), rel(s.esri)));
    out.push(format!("trusted:{}", rel(s.trusted_esri())));
    out.push(format!("geographic:{}", rel(Def::from_wkt(text).ok().and_then(|d| d.geographic_code()))));
    out.push(format!("shared:{}", rel(crate::identify_by_shared_name(text))));
    out.join(";")
}

#[test]
fn fitting_and_identifying_give_the_recorded_results() {
    let corpus = corpus(&[
        &include_bytes!("../../tests/data/projjson_corpus_epsg.jsonl.zst")[..],
        &include_bytes!("../../tests/data/projjson_corpus_esri.jsonl.zst")[..],
    ]);
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get().min(4));
    let chunk = corpus.len().div_ceil(threads);
    let found: Vec<(String, String, String)> = std::thread::scope(|s| {
        let handles: Vec<_> = corpus
            .chunks(chunk)
            .map(|part| {
                s.spawn(move || {
                    part.iter().map(|(code, form, text, _)| (outcomes(code, text), form.clone(), code.clone())).collect::<Vec<_>>()
                })
            })
            .collect();
        handles.into_iter().flat_map(|h| h.join().unwrap()).collect()
    });
    let mut groups: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for (o, form, code) in found {
        groups.entry((form, o)).or_default().push(code);
    }
    let mut out = String::new();
    for ((form, o), codes) in groups {
        writeln!(out, "{form}\t{o}\t{}", codes.join(" ")).unwrap();
    }
    check_recorded(BASELINE, &out);
}
