//! Source overview selection for warping, following GDAL's `-ovr` choices.

use crate::{Error, Result};

/// Which stored source resolution a warp reads. Overview levels start at zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OverviewSelection {
    /// Choose the finest stored resolution close to the output's sampling ratio.
    #[default]
    Auto,
    /// Choose `n` levels finer than the automatic choice (`AUTO-n`).
    AutoOffset(usize),
    /// Always read full resolution (`NONE`).
    None,
    /// Read this overview, or the last available one if the level is too large.
    Level(usize),
}

impl OverviewSelection {
    /// Parses `AUTO`, `AUTO-n`, `NONE`, or an overview number, case-insensitively.
    pub fn parse(value: &str) -> Result<Self> {
        let word = value.to_ascii_uppercase();
        match word.as_str() {
            "AUTO" => Ok(Self::Auto),
            "NONE" => Ok(Self::None),
            _ => {
                if let Some(offset) = word.strip_prefix("AUTO-") {
                    return offset.parse().map(Self::AutoOffset).map_err(|_| invalid(value));
                }
                word.parse().map(Self::Level).map_err(|_| invalid(value))
            }
        }
    }

    pub(crate) fn select(self, width: usize, overviews: &[(usize, usize)], target_ratio: f64) -> Option<usize> {
        match self {
            Self::None => None,
            Self::Level(index) => {
                if index >= overviews.len() {
                    crate::warnings::warn(format!("cannot get overview level {index}; using the coarsest available source resolution"));
                }
                (!overviews.is_empty()).then(|| index.min(overviews.len() - 1))
            }
            Self::Auto | Self::AutoOffset(_) => {
                if !target_ratio.is_finite() || target_ratio <= 1.0 {
                    return None;
                }
                let mut best = None;
                let mut best_ratio = 0.0;
                // GDAL includes full resolution in the search and uses 0.1 as
                // an absolute ratio tolerance, rather than rounding powers of two.
                for i in 0..=overviews.len() {
                    let ratio = if i == 0 { 1.0 } else { width as f64 / overviews[i - 1].0 as f64 };
                    if ratio >= target_ratio + 0.1 || ratio <= best_ratio {
                        continue;
                    }
                    best = i.checked_sub(1);
                    best_ratio = ratio;
                    if (target_ratio - ratio).abs() < 0.1 {
                        break;
                    }
                }
                let offset = match self {
                    Self::AutoOffset(n) => n,
                    _ => 0,
                };
                best.and_then(|index| index.checked_sub(offset))
            }
        }
    }
}

fn invalid(value: &str) -> Error {
    Error::InvalidArgument(format!("invalid overview selection '{value}' (AUTO, AUTO-n, NONE or a level from 0)"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn gdal_ratios_and_fallbacks() {
        let levels = [(512, 512), (256, 256), (128, 128)];
        assert_eq!(OverviewSelection::Auto.select(1024, &levels, 1.0), None);
        assert_eq!(OverviewSelection::Auto.select(1024, &levels, 1.95), Some(0));
        assert_eq!(OverviewSelection::Auto.select(1024, &levels, 3.0), Some(0));
        assert_eq!(OverviewSelection::Auto.select(1024, &levels, 4.0), Some(1));
        assert_eq!(OverviewSelection::AutoOffset(1).select(1024, &levels, 4.0), Some(0));
        assert_eq!(OverviewSelection::AutoOffset(3).select(1024, &levels, 4.0), None);
        assert_eq!(OverviewSelection::None.select(1024, &levels, 16.0), None);
        assert_eq!(OverviewSelection::Level(99).select(1024, &levels, 1.0), Some(2));
        assert_eq!(OverviewSelection::Level(0).select(1024, &[], 1.0), None);
        assert_eq!(OverviewSelection::parse("auto-2").unwrap(), OverviewSelection::AutoOffset(2));
        for invalid in ["-1", "AUTO-", "AUTO--1", "bilinear"] {
            assert!(OverviewSelection::parse(invalid).is_err());
        }
    }
}
