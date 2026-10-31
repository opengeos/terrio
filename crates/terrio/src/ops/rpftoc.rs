// SPDX-FileCopyrightText: Copyright (c) 2026, T-Kartor
// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
// Derived from GDAL 3.13.3 frmts/nitf/rpftocwriter.cpp and rpfframewriter.cpp (MIT).
//! Metadata-only CADRG frame inspection for RPF table-of-contents creation.
//!
//! Reference: GDAL 3.13.3, frmts/nitf/rpftocwriter.cpp and rpfframewriter.cpp
//! (Even Rouault, T-Kartor, MIT), and the MIL-STD-2411 RPF component layout.
//! Frame inspection seeks directly to bounded header/location/coverage records;
//! it never decodes image samples or reads the compressed pixel component.

use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::io::{Read, Seek};

mod encode;
mod frame;
mod grid;
#[cfg(not(target_family = "wasm"))]
mod native;
mod series;

/// Metadata needed to index one existing CADRG frame. Obtain it with
/// [read_frame]; image samples remain in the original frame file.
#[derive(Debug, Clone, Serialize)]
pub struct RpfFrame {
    /// Relative filename, including directories, as stored in the index.
    pub name: String,
    /// CADRG zone: 1–9 north, 10–18 south.
    pub zone: u8,
    /// Two-letter data series, used to infer scale unless explicitly supplied.
    pub series: String,
    /// Highest NITF/RPF frame classification, retained for the index section.
    pub classification: char,
    /// Four geographic corners (NW, SW, NE, SE), latitude then longitude,
    /// followed by latitude/longitude ground resolution and coordinate interval.
    pub coverage: [f64; 12],
}

impl RpfFrame {
    /// Reciprocal scale from the GDAL CADRG series catalogue, when fixed.
    /// Variable-scale and unknown series require an explicit scale.
    pub fn inferred_scale(&self) -> Option<u32> {
        series::scale(&self.series)
    }

    /// Validate the frame against a CADRG reciprocal scale and return its
    /// zero-based [column, bottom-up row] within the standard zone grid.
    /// Corners must agree within 0.001 frame; nonpolar angular intervals within
    /// one part per million. Legacy polar pitch/interval metadata is normalized.
    pub fn grid_position(&self, scale: u32) -> Result<[u32; 2]> {
        grid::Grid::new(self.zone, scale)?.locate(self)
    }
}

/// Inspect an NITF 2.0/2.1 CADRG frame using bounded metadata reads.
/// Names must be relative and contain a twelve-character frame basename.
/// Components ending in a dot or space are rejected for Windows portability.
/// NITF headers are limited to 1 MiB and location tables to 64 KiB.
pub fn read_frame(relative_name: &str, reader: &mut (impl Read + Seek)) -> Result<RpfFrame> {
    frame::read(relative_name, reader)
}

fn invalid(message: &str) -> Error {
    Error::InvalidData(format!("RPF: {message}"))
}

/// Options for a CADRG RPF table of contents. Input frames retain their own classification.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RpfTocOptions {
    /// Reciprocal scale override; otherwise inferred independently for each data series.
    pub scale: Option<u32>,
    /// Up to five printable ASCII bytes, used in the NITF header and boundary records.
    pub producer_id: String,
    /// Up to 27 printable ASCII bytes in the NITF originating-station name.
    pub producer_name: String,
    /// Empty or two uppercase ISO country letters.
    pub country_code: String,
    /// Index classification: U, R, C, S or T; must cover every frame classification.
    pub classification: char,
}
impl Default for RpfTocOptions {
    fn default() -> Self {
        Self { scale: None, producer_id: "TERRI".into(), producer_name: "Terrio".into(), country_code: String::new(), classification: 'U' }
    }
}

/// Serialize a metadata-only NITF 2.0 A.TOC referring to existing CADRG frame files.
/// Names are safe paths relative to the TOC directory. The 100,000-frame inspection cap and
/// 16 MiB metadata budget are resource limits; serialized capacity is lower and depends on the
/// paths/groups fitting a 99,999-byte TRE payload. Conflicting grid cells and inconsistent scale,
/// zone, corners, angular intervals or filename metadata fail before output.
/// Output resolution is derived from scale; legacy polar pitch/interval fields are normalized.
/// The file date-time is a fixed placeholder for reproducibility, not the creation time.
pub fn encode(frames: &[RpfFrame], options: &RpfTocOptions) -> Result<Vec<u8>> {
    encode::encode(frames, options)
}

/// Counts from native TOC creation. Metadata bytes exclude filesystem directory enumeration.
#[derive(Debug, Clone, Serialize)]
pub struct RpfTocReport {
    /// Number of indexed CADRG frame files.
    pub frames: usize,
    /// Non-frame candidates skipped, including unrelated files and prior indexes other than the output.
    pub skipped_files: usize,
    /// Number of distinct reciprocal-scale/zone mosaics.
    pub groups: usize,
    /// Bytes read from frame headers and required metadata components.
    pub metadata_bytes: u64,
    /// Size of the encoded TOC.
    pub output_bytes: usize,
}

/// Recursively inspect local CADRG files and atomically write an A.TOC (default:
/// input/A.TOC). Symlinks are skipped. Output must be in the input directory or
/// one of its ancestors, keeping all frame references traversal-free and portable.
/// All frames and the complete TOC are validated before creating the temporary file.
/// Input/output directory trees must remain stable during the call; callers must
/// synchronize concurrent directory changes. This is not a filesystem confinement
/// boundary. Atomic overwrite preserves Unix mode bits, not ownership, extended
/// ACLs or extended attributes.
#[cfg(not(target_family = "wasm"))]
pub fn create(input: &std::path::Path, output: Option<&std::path::Path>, options: &RpfTocOptions, overwrite: bool) -> Result<RpfTocReport> {
    native::create(input, output, options, overwrite)
}

#[cfg(test)]
pub(super) fn prepared_flat_frames(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut paths = Vec::new();
    for (directory, extension) in [("ON/RPF/ZONE2", "ON2"), ("TP/RPF/ZONE2", "TP2")] {
        let directory = root.join("rpftoc_landsat").join(directory);
        let before = paths.len();
        for entry in std::fs::read_dir(&directory).unwrap_or_else(|e| panic!("missing RPF fixture {}: {e}", directory.display())) {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|value| value == extension) {
                paths.push(path);
            }
        }
        assert!(paths.len() > before, "no {extension} fixtures in {}", directory.display());
    }
    paths.sort();
    paths
}
