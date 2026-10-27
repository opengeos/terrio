//! Editing vector datasets, in place or on output, as GDAL 3.13's `gdal vector rename-layer`,
//! `gdal vector edit` and `gdal vector update` do: renaming layers (to a given name, or with
//! ASCII, lower-case, file-name and length rules), changing a layer's CRS (without
//! reprojecting), declared geometry type, dataset and layer metadata, and updating a dataset
//! from another one, matching features by feature id or key fields.
//!
//! GeoPackages are changed in place: the file is read whole, changed and written to a temporary
//! file next to it, then renamed over the original, so that a failure leaves it as it was
//! (read-only files, files with a hot rollback journal or a write-ahead log are refused). Other
//! formats are rewritten the same way when updated, and renamed or edited on output.

use crate::crs::SpatialRef;
use crate::formats::VectorDriver;
use crate::{Error, Result};
use wbvector::{GeometryType, Layer};

// ------------------------------------------------------------------------------------------------
// Layer names
// ------------------------------------------------------------------------------------------------

/// How [`new_layer_names`] renames layers.
#[derive(Debug, Clone, Default)]
pub struct RenameOptions {
    /// The layer to rename to `output_layer` (needed when there are several layers).
    pub input_layer: Option<String>,
    /// The new name of `input_layer` (or of the only layer).
    pub output_layer: Option<String>,
    /// Replace accented Latin characters with ASCII ones, and other non-ASCII characters with
    /// `replacement_character` (or remove them).
    pub ascii: bool,
    /// Lower-case ASCII letters.
    pub lower_case: bool,
    /// Make names usable as file names on Linux, Windows and macOS.
    pub filename_compatible: bool,
    /// Characters to remove (or replace with `replacement_character`).
    pub reserved_characters: Option<String>,
    /// The character replacing removed ones.
    pub replacement_character: Option<char>,
    /// Truncate names to this many characters.
    pub max_length: Option<usize>,
}

/// GDAL's ASCII equivalents of the Latin-1 Supplement and Latin Extended-A characters
/// (`CPLUTF8ForceToASCII`, GDAL 3.13.3), by code point. GDAL maps U+00FF to `u`;
/// unsupported eth/thorn code points use the caller's replacement policy.
fn ascii_of(c: char) -> Option<&'static str> {
    const LATIN: [(u32, &str); 182] = [
        (0x00C0, "A"),
        (0x00C1, "A"),
        (0x00C2, "A"),
        (0x00C3, "A"),
        (0x00C4, "A"),
        (0x00C5, "A"),
        (0x00C6, "AE"),
        (0x00C7, "C"),
        (0x00C8, "E"),
        (0x00C9, "E"),
        (0x00CA, "E"),
        (0x00CB, "E"),
        (0x00CC, "I"),
        (0x00CD, "I"),
        (0x00CE, "I"),
        (0x00CF, "I"),
        (0x00D1, "N"),
        (0x00D2, "O"),
        (0x00D3, "O"),
        (0x00D4, "O"),
        (0x00D5, "O"),
        (0x00D6, "O"),
        (0x00D8, "O"),
        (0x00D9, "U"),
        (0x00DA, "U"),
        (0x00DB, "U"),
        (0x00DC, "U"),
        (0x00DD, "Y"),
        (0x00DF, "SS"),
        (0x00E0, "a"),
        (0x00E1, "a"),
        (0x00E2, "a"),
        (0x00E3, "a"),
        (0x00E4, "a"),
        (0x00E5, "a"),
        (0x00E6, "ae"),
        (0x00E7, "c"),
        (0x00E8, "e"),
        (0x00E9, "e"),
        (0x00EA, "e"),
        (0x00EB, "e"),
        (0x00EC, "i"),
        (0x00ED, "i"),
        (0x00EE, "i"),
        (0x00EF, "i"),
        (0x00F1, "n"),
        (0x00F2, "o"),
        (0x00F3, "o"),
        (0x00F4, "o"),
        (0x00F5, "o"),
        (0x00F6, "o"),
        (0x00F8, "o"),
        (0x00F9, "u"),
        (0x00FA, "u"),
        (0x00FB, "u"),
        (0x00FC, "u"),
        (0x00FD, "y"),
        (0x00FF, "u"),
        (0x0100, "A"),
        (0x0101, "a"),
        (0x0102, "A"),
        (0x0103, "a"),
        (0x0104, "A"),
        (0x0105, "a"),
        (0x0106, "C"),
        (0x0107, "c"),
        (0x0108, "C"),
        (0x0109, "c"),
        (0x010A, "C"),
        (0x010B, "c"),
        (0x010C, "C"),
        (0x010D, "c"),
        (0x010E, "D"),
        (0x010F, "d"),
        (0x0110, "D"),
        (0x0111, "d"),
        (0x0112, "E"),
        (0x0113, "e"),
        (0x0114, "E"),
        (0x0115, "e"),
        (0x0116, "E"),
        (0x0117, "e"),
        (0x0118, "E"),
        (0x0119, "e"),
        (0x011A, "E"),
        (0x011B, "e"),
        (0x011C, "G"),
        (0x011D, "g"),
        (0x011E, "G"),
        (0x011F, "g"),
        (0x0120, "G"),
        (0x0121, "g"),
        (0x0122, "G"),
        (0x0123, "g"),
        (0x0124, "H"),
        (0x0125, "h"),
        (0x0126, "H"),
        (0x0127, "h"),
        (0x0128, "I"),
        (0x0129, "i"),
        (0x012A, "I"),
        (0x012B, "i"),
        (0x012C, "I"),
        (0x012D, "i"),
        (0x012E, "I"),
        (0x012F, "i"),
        (0x0130, "I"),
        (0x0131, "i"),
        (0x0132, "IJ"),
        (0x0133, "ij"),
        (0x0134, "J"),
        (0x0135, "j"),
        (0x0136, "K"),
        (0x0137, "k"),
        (0x0138, "k"),
        (0x0139, "L"),
        (0x013A, "l"),
        (0x013B, "L"),
        (0x013C, "l"),
        (0x013D, "L"),
        (0x013E, "l"),
        (0x013F, "L"),
        (0x0140, "l"),
        (0x0141, "L"),
        (0x0142, "l"),
        (0x0143, "N"),
        (0x0144, "n"),
        (0x0145, "N"),
        (0x0146, "n"),
        (0x0147, "N"),
        (0x0148, "n"),
        (0x014C, "O"),
        (0x014D, "o"),
        (0x014E, "O"),
        (0x014F, "o"),
        (0x0150, "O"),
        (0x0151, "o"),
        (0x0152, "OE"),
        (0x0153, "oe"),
        (0x0154, "R"),
        (0x0155, "r"),
        (0x0156, "R"),
        (0x0157, "r"),
        (0x0158, "R"),
        (0x0159, "r"),
        (0x015A, "S"),
        (0x015B, "s"),
        (0x015C, "S"),
        (0x015D, "s"),
        (0x015E, "S"),
        (0x015F, "s"),
        (0x0160, "S"),
        (0x0161, "s"),
        (0x0162, "T"),
        (0x0163, "t"),
        (0x0164, "T"),
        (0x0165, "t"),
        (0x0166, "T"),
        (0x0167, "t"),
        (0x0168, "U"),
        (0x0169, "u"),
        (0x016A, "U"),
        (0x016B, "u"),
        (0x016C, "U"),
        (0x016D, "u"),
        (0x016E, "U"),
        (0x016F, "u"),
        (0x0170, "U"),
        (0x0171, "u"),
        (0x0172, "U"),
        (0x0173, "u"),
        (0x0174, "W"),
        (0x0175, "w"),
        (0x0176, "Y"),
        (0x0177, "y"),
        (0x0178, "Y"),
        (0x0179, "Z"),
        (0x017A, "z"),
        (0x017B, "Z"),
        (0x017C, "z"),
        (0x017D, "Z"),
        (0x017E, "z"),
    ];
    LATIN.binary_search_by_key(&(c as u32), |(p, _)| *p).ok().map(|i| LATIN[i].1)
}

/// `CPLLaunderForFilenameSafe`: characters invalid in file names replaced (or removed),
/// Windows device names and names ending with a space or dot suffixed.
fn launder_for_filename(s: &str, replacement: Option<char>) -> String {
    let mut out: String = s
        .chars()
        .filter_map(|c| {
            if (c as u32) < 32 || c as u32 == 127 || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' | '^') {
                replacement
            } else {
                Some(c)
            }
        })
        .collect();
    const RESERVED: [&str; 24] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4",
        "LPT5", "LPT6", "LPT7", "LPT8", "LPT9", "CONIN$", "CONOUT$",
    ];
    let suffix = replacement.unwrap_or('_');
    if RESERVED.iter().any(|r| r.eq_ignore_ascii_case(&out)) {
        out.push(suffix);
    }
    if out.ends_with(' ') || out.ends_with('.') {
        out.push(suffix);
    }
    out
}

/// The new names of layers, as `gdal vector rename-layer` makes them: `output_layer` for
/// `input_layer` (or the only layer), the others changed by the character rules (reserved
/// characters, then file-name compatibility, ASCII, lower case and length), names made
/// unique with `_N` suffixes when the rules made them equal.
pub fn new_layer_names(names: &[String], o: &RenameOptions) -> Result<Vec<String>> {
    if o.input_layer.is_some() && o.output_layer.is_none() {
        return Err(Error::InvalidArgument("the output layer name must be given with the input layer".into()));
    }
    if let Some(i) = &o.input_layer
        && !names.contains(i)
    {
        return Err(Error::InvalidArgument(format!("input layer '{i}' does not exist")));
    }
    if o.output_layer.is_some() && o.input_layer.is_none() && names.len() >= 2 {
        return Err(Error::InvalidArgument("the input layer must be given when renaming one of several layers".into()));
    }
    if o.max_length == Some(0) {
        return Err(Error::InvalidArgument("the maximum length must be at least 1".into()));
    }
    let truncate = |s: &mut String, n: usize| {
        if let Some((i, _)) = s.char_indices().nth(n) {
            s.truncate(i);
        }
    };
    let mut out = Vec::with_capacity(names.len());
    let mut counts: std::collections::HashMap<String, usize> = Default::default();
    let mut fixed = Vec::with_capacity(names.len());
    for name in names {
        if let Some(new) = &o.output_layer
            && (o.input_layer.as_ref() == Some(name) || names.len() == 1)
        {
            out.push(new.clone());
            fixed.push(true);
            continue;
        }
        let mut s = name.clone();
        if let Some(reserved) = &o.reserved_characters {
            s = s.chars().filter_map(|c| if reserved.contains(c) { o.replacement_character } else { Some(c) }).collect();
        }
        if o.filename_compatible {
            s = launder_for_filename(&s, o.replacement_character);
        }
        if o.ascii {
            let mut a = String::with_capacity(s.len());
            for c in s.chars() {
                if c.is_ascii() {
                    a.push(c);
                } else if let Some(r) = ascii_of(c) {
                    a.push_str(r);
                } else if let Some(r) = o.replacement_character {
                    a.push(r);
                }
            }
            s = a;
        }
        if o.lower_case {
            s.make_ascii_lowercase();
        }
        if let Some(n) = o.max_length {
            truncate(&mut s, n);
        }
        *counts.entry(s.clone()).or_default() += 1;
        out.push(s);
        fixed.push(false);
    }
    // Names made equal get `_1`, `_2`... (`_01` from 10 equal names, `_001` from 100).
    let mut seen: std::collections::HashMap<String, usize> = Default::default();
    for (s, f) in out.iter_mut().zip(&fixed) {
        let n = counts.get(s.as_str()).copied().unwrap_or(0);
        if *f || n < 2 {
            continue;
        }
        let k = seen.entry(s.clone()).or_default();
        *k += 1;
        let suffix = match n {
            ..=9 => format!("_{k}"),
            10..=99 => format!("_{k:02}"),
            _ => format!("_{k:03}"),
        };
        let len = s.chars().count();
        match o.max_length {
            Some(max) if len + suffix.len() > max => {
                if len > suffix.len() {
                    truncate(s, len - suffix.len());
                    s.push_str(&suffix);
                } else {
                    crate::warnings::warn(format!("cannot make a unique name for '{s}' within {max} characters"));
                }
            }
            _ => s.push_str(&suffix),
        }
    }
    let mut unique = std::collections::HashSet::new();
    for name in &out {
        if !unique.insert(name) {
            return Err(Error::InvalidArgument(format!(
                "renaming would create duplicate layer name '{name}'; choose a distinct output name"
            )));
        }
    }

    Ok(out)
}

/// Whether a path names a local GeoPackage file.
fn local_gpkg(path: &str) -> bool {
    VectorDriver::for_extension(path) == Some(VectorDriver::GPKG)
        && crate::vfs::Location::parse(path).is_local()
        && std::path::Path::new(path).is_file()
}

/// Renames the layers of a GeoPackage in place (see [`new_layer_names`]), with their indexes,
/// triggers and the rows describing them. Returns the `(old, new)` names that changed. Other
/// formats are renamed on output, with [`rename_layers`] and a write.
pub fn rename_layers_in_place(path: &str, o: &RenameOptions) -> Result<Vec<(String, String)>> {
    #[cfg(not(feature = "sqlite"))]
    let _ = o;
    if !local_gpkg(path) {
        return Err(Error::Unsupported(format!("'{path}': layers are renamed in place in local GeoPackages only; give an output")));
    }
    #[cfg(feature = "sqlite")]
    {
        use crate::drivers::vector::gpkg_edit as ge;
        return ge::modify_file(path, |d| {
            let names = ge::layer_names(d);
            let new = new_layer_names(&names, o)?;
            let renamed: Vec<(String, String)> = names.into_iter().zip(new).filter(|(a, b)| a != b).collect();
            // Through temporary names, so that names can be swapped.
            for (i, (old, _)) in renamed.iter().enumerate() {
                ge::rename_layer(d, old, &format!("__terrio_rename_{i}"))?;
            }
            for (i, (_, new)) in renamed.iter().enumerate() {
                ge::rename_layer(d, &format!("__terrio_rename_{i}"), new)?;
            }
            Ok(renamed)
        });
    }
    #[allow(unreachable_code)]
    Err(Error::disabled("sqlite"))
}

/// Renames layers in memory (see [`new_layer_names`]).
pub fn rename_layers(layers: &mut [Layer], o: &RenameOptions) -> Result<()> {
    let names: Vec<String> = layers.iter().map(|l| l.name.clone()).collect();
    for (l, n) in layers.iter_mut().zip(new_layer_names(&names, o)?) {
        l.name = n;
    }
    Ok(())
}

// ------------------------------------------------------------------------------------------------
// Edit
// ------------------------------------------------------------------------------------------------

/// What [`edit_in_place`] and [`edit_layers`] change.
#[derive(Debug, Clone, Default)]
pub struct EditOptions {
    /// Change only this layer (others are kept). By default, every layer.
    pub active_layer: Option<String>,
    /// The CRS to assign without reprojecting: `Some(None)` removes it.
    pub crs: Option<Option<SpatialRef>>,
    /// The declared geometry type (`None` for any), with Z and M. Geometries are not changed.
    pub geometry_type: Option<(Option<GeometryType>, bool, bool)>,
    /// Dataset metadata items to set.
    pub metadata: Vec<(String, String)>,
    /// Dataset metadata items to remove.
    pub unset_metadata: Vec<String>,
    /// Layer metadata items to set.
    pub layer_metadata: Vec<(String, String)>,
    /// Layer metadata items to remove.
    pub unset_layer_metadata: Vec<String>,
    /// Renumber features and drop the FID column name (on output only).
    pub unset_fid: bool,
}

impl EditOptions {
    fn has_metadata(&self) -> bool {
        !(self.metadata.is_empty()
            && self.unset_metadata.is_empty()
            && self.layer_metadata.is_empty()
            && self.unset_layer_metadata.is_empty())
    }
}

/// Parses `KEY=VALUE` metadata items.
pub fn parse_metadata_items(items: &[String]) -> Result<Vec<(String, String)>> {
    items
        .iter()
        .map(|s| {
            s.split_once('=')
                .filter(|(k, _)| !k.is_empty())
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .ok_or_else(|| Error::InvalidArgument(format!("metadata item '{s}': expected KEY=VALUE")))
        })
        .collect()
}

/// Parses a CRS option as `gdal vector edit --crs` reads it: `null` or `none` removes the CRS.
pub fn parse_crs_override(s: &str) -> Result<Option<SpatialRef>> {
    if s.eq_ignore_ascii_case("null") || s.eq_ignore_ascii_case("none") { Ok(None) } else { SpatialRef::parse(s).map(Some) }
}

fn edit_layer_name<'a>(names: impl Iterator<Item = &'a str>, requested: &str) -> Result<String> {
    let mut insensitive = None;
    for name in names {
        if name == requested {
            return Ok(name.to_string());
        }
        if insensitive.is_none() && name.eq_ignore_ascii_case(requested) {
            insensitive = Some(name);
        }
    }
    insensitive.map(str::to_string).ok_or_else(|| Error::InvalidArgument(format!("cannot find layer '{requested}'")))
}

/// Edits layers in memory: CRS (assigned, not reprojected), declared geometry type and feature
/// ids. Metadata lives in files: see [`edit_in_place`].
pub fn edit_layers(layers: &mut [Layer], o: &EditOptions) -> Result<()> {
    let active = o.active_layer.as_deref().map(|name| edit_layer_name(layers.iter().map(|layer| layer.name.as_str()), name)).transpose()?;
    for l in layers.iter_mut().filter(|l| active.as_ref().is_none_or(|name| &l.name == name)) {
        match &o.crs {
            Some(Some(s)) => s.apply_to_layer(l),
            Some(None) => {
                l.set_crs_epsg(None);
                l.set_crs_wkt(None);
            }
            None => {}
        }
        if let Some((t, _, _)) = o.geometry_type {
            l.geom_type = t;
        }
        if o.unset_fid {
            for (i, f) in l.features.iter_mut().enumerate() {
                f.fid = i as u64 + 1;
            }
            // A field named fid would give the feature ids of GeoPackage outputs.
            if let Some(i) = l.schema.fields().iter().position(|f| f.name.eq_ignore_ascii_case("fid")) {
                let mut schema = wbvector::Schema::new();
                for (k, f) in l.schema.fields().iter().enumerate() {
                    if k != i {
                        schema.add_field(f.clone());
                    }
                }
                l.schema = schema;
                for f in &mut l.features {
                    if i < f.attributes.len() {
                        f.attributes.remove(i);
                    }
                }
            }
        }
    }
    Ok(())
}

/// Edits a GeoPackage in place, as `gdal vector edit` edits a dataset: the CRS of layers
/// (assigned without reprojecting: their SRS, and that of every geometry), their declared
/// geometry type (with Z and M), and dataset and layer metadata (GDAL's metadata XML, a
/// layer's `DESCRIPTION` and `IDENTIFIER` in `gpkg_contents`). Features are not changed.
pub fn edit_in_place(path: &str, o: &EditOptions) -> Result<()> {
    if !local_gpkg(path) {
        return Err(Error::Unsupported(format!("'{path}': datasets are edited in place in local GeoPackages only; give an output")));
    }
    if o.unset_fid {
        return Err(Error::InvalidArgument("unsetting feature ids needs an output dataset".into()));
    }
    #[cfg(feature = "sqlite")]
    {
        use crate::drivers::vector::gpkg_edit as ge;
        return ge::modify_file(path, |d| {
            let names = ge::layer_names(d);
            let active = o.active_layer.as_deref().map(|name| edit_layer_name(names.iter().map(String::as_str), name)).transpose()?;
            for n in names.iter().filter(|n| active.as_ref().is_none_or(|name| name == *n)) {
                let spatial = ge::has_geometry(d, n) || o.active_layer.is_some();
                if spatial && let Some(c) = &o.crs {
                    ge::set_layer_crs(d, n, c.as_ref())?;
                }
                if spatial && let Some((t, z, m)) = o.geometry_type {
                    ge::set_geometry_type(d, n, t, z, m)?;
                }
                ge::edit_metadata(d, Some(n), &o.layer_metadata, &o.unset_layer_metadata)?;
            }
            ge::edit_metadata(d, None, &o.metadata, &o.unset_metadata)
        });
    }
    #[allow(unreachable_code)]
    Err(Error::disabled("sqlite"))
}

/// Edits a dataset on output, as `gdal vector edit input output` does: the layers of `input`
/// edited ([`edit_layers`]) and written to `output`; for a GeoPackage output, the metadata of a
/// GeoPackage input's default domain is kept and edited, and geometry type Z/M are recorded.
/// Named domains are not copied to a new output, matching GDAL 3.13.3.
pub fn edit_to(input: &str, output: &str, o: &EditOptions, write: &crate::VectorWriteOptions) -> Result<()> {
    if input == output || std::fs::canonicalize(input).ok().zip(std::fs::canonicalize(output).ok()).is_some_and(|(a, b)| a == b) {
        return Err(Error::InvalidArgument("input and output datasets must be different; use edit_in_place".into()));
    }
    let mut layers = crate::open_vector(input, &Default::default())?.layers;
    edit_layers(&mut layers, o)?;
    let out_gpkg = write.driver.or_else(|| VectorDriver::for_extension(output)) == Some(VectorDriver::GPKG);
    if (o.has_metadata() || o.geometry_type.is_some_and(|(_, z, m)| z || m)) && !out_gpkg {
        return Err(Error::Unsupported("metadata and Z/M geometry types are written to GeoPackage outputs only".into()));
    }
    crate::write_vector(&layers, output, write)?;
    if !out_gpkg {
        return Ok(());
    }
    #[cfg(feature = "sqlite")]
    {
        use crate::drivers::vector::gpkg_edit as ge;
        // The input's metadata, carried over, then edited.
        let (ds_md, layer_md) = if local_gpkg(input) {
            let bytes = std::fs::read(input)?;
            ge::modify_bytes(bytes, |d| {
                let names = ge::layer_names(d);
                let per_layer: Vec<(String, ge::Metadata)> = names.iter().map(|n| (n.clone(), ge::metadata(d, Some(n)))).collect();
                Ok((ge::metadata(d, None), per_layer))
            })?
            .1
        } else {
            (Vec::new(), Vec::new())
        };
        let default = |md: &ge::Metadata| md.iter().find(|(dom, _)| dom.is_empty()).map(|(_, i)| i.clone()).unwrap_or_default();
        return ge::modify_file(output, |d| {
            let names = ge::layer_names(d);
            ge::edit_metadata(d, None, &default(&ds_md), &[])?;
            for (n, md) in &layer_md {
                if names.contains(n) {
                    ge::edit_metadata(d, Some(n), &default(md), &[])?;
                }
            }
            let active = o.active_layer.as_deref().map(|name| edit_layer_name(names.iter().map(String::as_str), name)).transpose()?;
            for n in names.iter().filter(|n| active.as_ref().is_none_or(|name| name == *n)) {
                if (ge::has_geometry(d, n) || o.active_layer.is_some())
                    && let Some((t, z, m)) = o.geometry_type
                {
                    ge::set_geometry_type(d, n, t, z, m)?;
                }
                ge::edit_metadata(d, Some(n), &o.layer_metadata, &o.unset_layer_metadata)?;
            }
            ge::edit_metadata(d, None, &o.metadata, &o.unset_metadata)
        });
    }
    #[allow(unreachable_code)]
    Err(Error::disabled("sqlite"))
}

/// GDAL's metadata of a GeoPackage: the dataset's default-domain items, and each layer's
/// (with `DESCRIPTION` and `IDENTIFIER` from `gpkg_contents`).
pub type GpkgMetadata = (Vec<(String, String)>, Vec<(String, Vec<(String, String)>)>);

/// Reads GDAL's metadata of a local GeoPackage (see [`GpkgMetadata`]).
pub fn read_gpkg_metadata(path: &str) -> Result<GpkgMetadata> {
    #[cfg(feature = "sqlite")]
    {
        use crate::drivers::vector::gpkg_edit as ge;
        let default = |md: ge::Metadata| md.into_iter().find(|(dom, _)| dom.is_empty()).map(|(_, i)| i).unwrap_or_default();
        return ge::modify_bytes(std::fs::read(path)?, |d| {
            let names = ge::layer_names(d);
            Ok((default(ge::metadata(d, None)), names.iter().map(|n| (n.clone(), default(ge::metadata(d, Some(n))))).collect()))
        })
        .map(|(_, md)| md);
    }
    #[allow(unreachable_code)]
    {
        let _ = path;
        Err(Error::disabled("sqlite"))
    }
}

// ------------------------------------------------------------------------------------------------
// Update
// ------------------------------------------------------------------------------------------------

/// How [`update`] writes the input's features.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UpdateMode {
    /// Update matched features and append the others.
    #[default]
    Merge,
    /// Update matched features only.
    UpdateOnly,
    /// Append unmatched features only.
    AppendOnly,
}

impl UpdateMode {
    /// Parses `merge`, `update-only` or `append-only`.
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "merge" => Self::Merge,
            "update-only" => Self::UpdateOnly,
            "append-only" => Self::AppendOnly,
            _ => return Err(Error::InvalidArgument(format!("unknown update mode '{s}': expected merge, update-only or append-only"))),
        })
    }
}

/// Options of [`update`].
#[derive(Debug, Clone, Default)]
pub struct UpdateOptions {
    /// The input layer (needed when the input has several).
    pub input_layer: Option<String>,
    /// The output layer (needed when the output has several).
    pub output_layer: Option<String>,
    /// Key fields matching features (feature ids by default).
    pub key: Vec<String>,
    /// How features are written.
    pub mode: UpdateMode,
}

/// Counts of what an update did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UpdateCounts {
    /// Features updated (changed).
    pub updated: usize,
    /// Features appended.
    pub appended: usize,
    /// Input features matched to an output feature they left unchanged, or skipped.
    pub unchanged: usize,
}

/// A key value, comparable across field and SQLite values.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum KeyValue {
    Int(i64),
    Real(u64),
    Text(String),
}

/// The key of a field value (`None` for null values and other types).
pub(crate) fn key_of_field(v: &wbvector::FieldValue) -> Option<KeyValue> {
    use wbvector::FieldValue as V;
    Some(match v {
        V::Integer(i) => KeyValue::Int(*i),
        V::Boolean(b) => KeyValue::Int(i64::from(*b)),
        V::Float(f) => KeyValue::Real(f.to_bits()),
        V::Text(s) | V::Date(s) | V::DateTime(s) => KeyValue::Text(s.clone()),
        _ => return None,
    })
}

/// The layer of `layers` named `name`, or the only one.
fn pick<'a>(layers: &'a [Layer], name: Option<&str>, what: &str) -> Result<&'a Layer> {
    match name {
        Some(n) => {
            layers.iter().find(|l| l.name == n).ok_or_else(|| Error::InvalidArgument(format!("no layer named '{n}' in {what} dataset")))
        }
        None if layers.len() == 1 => Ok(&layers[0]),
        None => Err(Error::InvalidArgument(format!("the {what} dataset has {} layers: name the {what} layer", layers.len()))),
    }
}

/// Updates one selected destination layer in memory, preserving the other layers.
pub fn update_layers(dst: &mut [Layer], src: &[Layer], o: &UpdateOptions) -> Result<UpdateCounts> {
    let source = pick(src, o.input_layer.as_deref(), "input")?;
    let name = pick(dst, o.output_layer.as_deref(), "output")?.name.clone();
    let target = dst.iter_mut().find(|l| l.name == name).expect("selected layer");
    update_layer(target, source, o)
}

/// Updates layer `dst` in memory from `src`, as [`update`] does.
pub fn update_layer(dst: &mut Layer, src: &Layer, o: &UpdateOptions) -> Result<UpdateCounts> {
    let mut counts = UpdateCounts::default();
    let sf = src.schema.fields();
    let df = dst.schema.fields().to_vec();
    let mut keys = Vec::new();
    for k in &o.key {
        let si = sf
            .iter()
            .position(|f| f.name.eq_ignore_ascii_case(k))
            .ok_or_else(|| Error::InvalidArgument(format!("cannot find field '{k}' in input layer")))?;
        if !matches!(sf[si].field_type, wbvector::FieldType::Integer | wbvector::FieldType::Float | wbvector::FieldType::Text) {
            return Err(Error::Unsupported(format!(
                "type of field '{k}' is not one of those supported for a key field: String, Integer, Integer64, Real"
            )));
        }
        let di = df
            .iter()
            .position(|f| f.name.eq_ignore_ascii_case(k))
            .ok_or_else(|| Error::InvalidArgument(format!("cannot find field '{k}' in output layer")))?;
        if df[di].field_type != sf[si].field_type {
            return Err(Error::Unsupported(format!("type of field '{k}' is not the same in input and output layers")));
        }
        keys.push((si, di));
    }
    let mut index: std::collections::HashMap<Vec<KeyValue>, Vec<usize>> = Default::default();
    let mut by_fid: std::collections::HashMap<u64, usize> = Default::default();
    for (i, f) in dst.features.iter().enumerate() {
        by_fid.insert(f.fid, i);
        if !keys.is_empty()
            && let Some(k) = keys.iter().map(|(_, di)| f.attributes.get(*di).and_then(key_of_field)).collect::<Option<Vec<_>>>()
        {
            index.entry(k).or_default().push(i);
        }
    }
    // For each output field, the input field giving its values (by name, ignoring case).
    let map: Vec<Option<usize>> = df.iter().map(|d| sf.iter().position(|s| s.name.eq_ignore_ascii_case(&d.name))).collect();
    let src_types: Vec<_> = (0..sf.len()).map(|i| crate::ops::vector_schema::ogr_type(src, i)).collect();
    let dst_types: Vec<_> = (0..df.len()).map(|i| crate::ops::vector_schema::ogr_type(dst, i)).collect();
    let mut next = dst.features.iter().map(|f| f.fid).max().unwrap_or(0) + 1;
    let mut appended = Vec::new();
    for f in &src.features {
        let target = if keys.is_empty() {
            by_fid.get(&f.fid).copied()
        } else {
            let key = keys.iter().map(|(si, _)| f.attributes.get(*si).and_then(key_of_field)).collect::<Option<Vec<_>>>();
            match key.as_ref().and_then(|key| index.get(key)).map(Vec::as_slice) {
                Some([i]) => Some(*i),
                _ => None,
            }
        };
        let values = |base: &[wbvector::FieldValue]| -> Vec<wbvector::FieldValue> {
            let mut v = base.to_vec();
            v.resize(df.len(), wbvector::FieldValue::Null);
            for (i, m) in map.iter().enumerate() {
                if let Some(si) = m {
                    v[i] = match f.attributes.get(*si) {
                        Some(x) => crate::ops::vector_schema::convert_value(x, &sf[*si], src_types[*si], dst_types[i]),
                        None => wbvector::FieldValue::Null,
                    };
                }
            }
            v
        };
        match target {
            Some(i) if o.mode != UpdateMode::AppendOnly => {
                let new = values(&dst.features[i].attributes);
                if new == dst.features[i].attributes && f.geometry == dst.features[i].geometry {
                    counts.unchanged += 1;
                } else {
                    dst.features[i].attributes = new;
                    dst.features[i].geometry = f.geometry.clone();
                    counts.updated += 1;
                }
            }
            None if o.mode != UpdateMode::UpdateOnly => {
                let attrs = values(&[]);
                appended.push(wbvector::Feature { fid: next, geometry: f.geometry.clone(), attributes: attrs });
                next += 1;
                counts.appended += 1;
            }
            _ => counts.unchanged += 1,
        }
    }
    dst.features.extend(appended);
    dst.extent = None;
    Ok(counts)
}

/// Updates an existing dataset from the layer of an input, as `gdal vector update` does: each
/// input feature is matched with the output feature of the same feature id, or of the same
/// values of the `key` fields (String, Integer or Real fields of the same type in both
/// layers; features with a null key match nothing and are appended unless update-only; a key matching several features matches
/// none). Matched features take the input's geometry and the values of the fields both layers
/// have (fields only the output has are kept, those only the input has are not added);
/// unmatched features are appended with new feature ids. `mode` keeps only updates or only
/// appends. No reprojection is done. GeoPackages are changed in place; other formats are
/// rewritten, the new files replacing the old ones once complete.
pub fn update(src: &[Layer], output: &str, o: &UpdateOptions) -> Result<UpdateCounts> {
    #[cfg(feature = "sqlite")]
    let source = pick(src, o.input_layer.as_deref(), "input")?;
    #[cfg(not(feature = "sqlite"))]
    pick(src, o.input_layer.as_deref(), "input")?;
    if !crate::vfs::Location::parse(output).is_local() {
        return Err(Error::Unsupported(format!("'{output}': only local datasets can be updated")));
    }
    if !std::path::Path::new(output).exists() {
        return Err(Error::Io(std::io::Error::new(std::io::ErrorKind::NotFound, format!("'{output}' does not exist"))));
    }
    #[cfg(feature = "sqlite")]
    if local_gpkg(output) {
        use crate::drivers::vector::gpkg_edit as ge;
        let mode = match o.mode {
            UpdateMode::Merge => ge::Upsert::Merge,
            UpdateMode::UpdateOnly => ge::Upsert::UpdateOnly,
            UpdateMode::AppendOnly => ge::Upsert::AppendOnly,
        };
        return ge::modify_file(output, |d| {
            let names = ge::layer_names(d);
            let name = match &o.output_layer {
                Some(n) => n.clone(),
                None if names.len() == 1 => names[0].clone(),
                None => {
                    return Err(Error::InvalidArgument(format!("the output dataset has {} layers: name the output layer", names.len())));
                }
            };
            ge::update_features(d, &name, source, &o.key, mode)
        });
    }
    let ds = crate::open_vector(output, &Default::default())?;
    let mut layers = ds.layers;
    let counts = update_layers(&mut layers, src, o)?;
    replace_dataset(&layers, output, &crate::VectorWriteOptions { driver: Some(ds.driver), ..Default::default() })?;
    Ok(counts)
}

/// Stages a replacement beside the original, then renames its files into place.
/// Multi-file formats are not atomic across sidecars; an error can leave a partial replacement.
/// each file renamed over the one of the same name (a shapefile's parts too).
#[cfg(not(target_family = "wasm"))]
fn replace_dataset(layers: &[Layer], path: &str, write: &crate::VectorWriteOptions) -> Result<()> {
    let p = std::path::Path::new(path);
    if std::fs::metadata(p)?.permissions().readonly() {
        return Err(Error::Unsupported(format!("'{path}' is read-only")));
    }
    let parent = p.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(std::path::Path::new("."));
    let file = p.file_name().ok_or_else(|| Error::InvalidArgument(format!("'{path}' has no file name")))?;
    let staging = tempfile::tempdir_in(parent)?;
    let tmp = staging.path();
    (|| -> Result<()> {
        let target = tmp.join(file);
        crate::write_vector(layers, &target.to_string_lossy(), write)?;
        let staged: std::collections::HashSet<_> =
            std::fs::read_dir(tmp)?.map(|entry| entry.map(|entry| entry.file_name())).collect::<std::io::Result<_>>()?;
        for e in std::fs::read_dir(tmp)? {
            let e = e?;
            std::fs::rename(e.path(), parent.join(e.file_name()))?;
        }
        if write.driver.or_else(|| VectorDriver::for_extension(path)) == Some(VectorDriver::Shapefile) {
            // Old indexes/optional sidecars do not describe the new geometry and attributes.
            for extension in ["qix", "sbn", "sbx", "prj", "cpg"] {
                let sidecar = p.with_extension(extension);
                if !staged.contains(sidecar.file_name().unwrap()) {
                    match std::fs::remove_file(&sidecar) {
                        Ok(()) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error.into()),
                    }
                }
            }
        }
        Ok(())
    })()
}

#[cfg(target_family = "wasm")]
fn replace_dataset(_layers: &[Layer], _path: &str, _write: &crate::VectorWriteOptions) -> Result<()> {
    Err(Error::Unsupported("file updates are unavailable in WebAssembly; use update_layers".into()))
}

#[cfg(test)]
mod tests {
    #[test]
    fn latin_y_diaeresis_matches_gdal_313() {
        assert_eq!(super::ascii_of('ÿ'), Some("u"));
    }

    use super::*;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn renames_with_gdal_rules() {
        let o = RenameOptions {
            ascii: true,
            lower_case: true,
            filename_compatible: true,
            reserved_characters: Some(" ".into()),
            ..Default::default()
        };
        assert_eq!(new_layer_names(&names(&["Café Été", "a/b:c", "CON"]), &o).unwrap(), names(&["cafeete", "abc", "con_"]));
        let o = RenameOptions { replacement_character: Some('_'), filename_compatible: true, ..Default::default() };
        assert_eq!(new_layer_names(&names(&["a/b", "a:b", "x."]), &o).unwrap(), names(&["a_b_1", "a_b_2", "x._"]));
        let o = RenameOptions { max_length: Some(3), ..Default::default() };
        assert_eq!(new_layer_names(&names(&["abcd", "abce", "xy"]), &o).unwrap(), names(&["a_1", "a_2", "xy"]));
        let o = RenameOptions { input_layer: Some("b".into()), output_layer: Some("new".into()), ..Default::default() };
        assert_eq!(new_layer_names(&names(&["a", "b"]), &o).unwrap(), names(&["a", "new"]));
        assert!(new_layer_names(&names(&["a", "b"]), &RenameOptions { output_layer: Some("x".into()), ..Default::default() }).is_err());
        assert!(new_layer_names(&names(&["a"]), &RenameOptions { input_layer: Some("a".into()), ..Default::default() }).is_err());
        assert_eq!(ascii_of('Ł'), Some("L"));
        assert_eq!(ascii_of('ž'), Some("z"));
        assert_eq!(ascii_of('Œ'), Some("OE"));
    }

    #[test]
    fn selected_rename_rejects_collision_with_a_retained_layer() {
        let options = RenameOptions { input_layer: Some("a".into()), output_layer: Some("b".into()), ..Default::default() };
        for input in [names(&["a", "b"]), names(&["b", "a"])] {
            assert!(new_layer_names(&input, &options).unwrap_err().to_string().contains("duplicate layer name"));
        }
    }

    #[test]
    fn parses_edit_options() {
        assert_eq!(parse_metadata_items(&["A=1=2".into()]).unwrap(), [("A".to_string(), "1=2".to_string())]);
        assert!(parse_metadata_items(&["nokey".into()]).is_err());
        assert!(parse_crs_override("none").unwrap().is_none());
        assert_eq!(parse_crs_override("EPSG:4326").unwrap().unwrap().epsg, Some(4326));
        assert_eq!(UpdateMode::parse("update-only").unwrap(), UpdateMode::UpdateOnly);
    }

    #[cfg(not(target_family = "wasm"))]
    #[test]
    fn successful_shapefile_replacement_removes_stale_indexes() {
        use wbvector::{Coord, Feature, Geometry};
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("points.shp");
        let mut layer = Layer::new("points").with_geom_type(GeometryType::Point);
        layer.push(Feature { fid: 1, geometry: Some(Geometry::Point(Coord::xy(1., 2.))), attributes: vec![] });
        let write = crate::VectorWriteOptions { driver: Some(VectorDriver::Shapefile), ..Default::default() };
        crate::write_vector(&[layer.clone()], path.to_str().unwrap(), &write).unwrap();
        for extension in ["qix", "sbn", "sbx", "prj"] {
            std::fs::write(path.with_extension(extension), b"stale").unwrap();
        }
        replace_dataset(&[layer], path.to_str().unwrap(), &write).unwrap();
        for extension in ["qix", "sbn", "sbx", "prj"] {
            assert!(!path.with_extension(extension).exists(), "{extension}");
        }
        assert_eq!(crate::open_vector(path.to_str().unwrap(), &Default::default()).unwrap().layers[0].len(), 1);
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn edits_select_layers_case_insensitively() {
        use crate::drivers::vector::geopackage as gw;
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("source.gpkg");
        let output = temp.path().join("copy.gpkg");
        let layer = Layer::new("roads").with_geom_type(GeometryType::Point);
        std::fs::write(&input, gw::to_bytes(&[layer]).unwrap()).unwrap();
        let mut options =
            EditOptions { active_layer: Some("ROADS".into()), crs: Some(Some(SpatialRef::from_epsg(4326).unwrap())), ..Default::default() };
        edit_in_place(input.to_str().unwrap(), &options).unwrap();
        options.crs = Some(Some(SpatialRef::from_epsg(3857).unwrap()));
        edit_to(input.to_str().unwrap(), output.to_str().unwrap(), &options, &Default::default()).unwrap();
        assert_eq!(crate::open_vector(output.to_str().unwrap(), &Default::default()).unwrap().layers[0].crs_epsg(), Some(3857));
        assert_eq!(edit_layer_name(["roads", "ROADS"].into_iter(), "ROADS").unwrap(), "ROADS");
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn default_gpkg_edit_skips_attribute_tables() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("mixed.gpkg");
        let spatial = Layer::new("spatial").with_geom_type(GeometryType::Point);
        let attributes = Layer::new("attributes");
        crate::write_vector(&[spatial, attributes], path.to_str().unwrap(), &Default::default()).unwrap();
        let options = EditOptions { crs: Some(Some(SpatialRef::from_epsg(4326).unwrap())), ..Default::default() };
        edit_in_place(path.to_str().unwrap(), &options).unwrap();
        let layers = crate::open_vector(path.to_str().unwrap(), &Default::default()).unwrap().layers;
        assert_eq!(layers.iter().find(|layer| layer.name == "spatial").unwrap().crs_epsg(), Some(4326));
        assert!(layers.iter().find(|layer| layer.name == "attributes").unwrap().geom_type.is_none());
        assert!(edit_in_place(path.to_str().unwrap(), &EditOptions { active_layer: Some("attributes".into()), ..options }).is_err());
    }

    #[test]
    fn updates_layers_in_memory() {
        use wbvector::{Feature, FieldDef, FieldType, FieldValue, Geometry};
        let mut dst = Layer::new("d");
        dst.add_field(FieldDef::new("id", FieldType::Text));
        dst.add_field(FieldDef::new("keep", FieldType::Integer));
        for (fid, id) in [(1, "a"), (2, "b"), (3, "b")] {
            dst.push(Feature {
                fid,
                geometry: Some(Geometry::point(0.0, 0.0)),
                attributes: vec![FieldValue::Text(id.into()), FieldValue::Integer(7)],
            });
        }
        let mut src = Layer::new("s");
        src.add_field(FieldDef::new("ID", FieldType::Text));
        for (fid, id) in [(9, "a"), (8, "b"), (7, "c")] {
            src.push(Feature { fid, geometry: Some(Geometry::point(1.0, 1.0)), attributes: vec![FieldValue::Text(id.into())] });
        }
        let o = UpdateOptions { key: vec!["id".into()], ..Default::default() };
        let counts = update_layer(&mut dst, &src, &o).unwrap();
        // "a" updated; "b" matches two features, so it is appended, as "c" is.
        assert_eq!(counts, UpdateCounts { updated: 1, appended: 2, unchanged: 0 });
        assert_eq!(dst.len(), 5);
        assert_eq!(dst.features[0].attributes[1], FieldValue::Integer(7), "fields only the output has are kept");
        assert_eq!(dst.features[3].fid, 4);
        assert!(update_layer(&mut dst, &src, &UpdateOptions { key: vec!["keep".into()], ..Default::default() }).is_err());
        let mut null_source = src.clone();
        null_source.features.truncate(1);
        null_source.features[0].attributes[0] = FieldValue::Null;
        for mode in [UpdateMode::Merge, UpdateMode::UpdateOnly, UpdateMode::AppendOnly] {
            let mut target = dst.clone();
            target.features[0].attributes[0] = FieldValue::Null;
            let counts = update_layer(&mut target, &null_source, &UpdateOptions { mode, ..o.clone() }).unwrap();
            assert_eq!(counts.updated, 0);
            assert_eq!(counts.appended, usize::from(mode != UpdateMode::UpdateOnly));
        }
    }
}
