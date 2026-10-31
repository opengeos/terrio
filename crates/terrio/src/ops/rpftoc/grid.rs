// SPDX-FileCopyrightText: 2026 Qiusheng Wu
// SPDX-FileCopyrightText: 2026 T-Kartor
// SPDX-License-Identifier: MIT
//! MIL-C-89038 CADRG grids. Formula reference: GDAL rpfframewriter.cpp (MIT).
use super::{RpfFrame, invalid};
use crate::{Error, Result};

const FRAME: f64 = 1536.;
const RADIUS: f64 = 6_378_137.;
// minimum/maximum latitude, longitudinal constant, latitude pitch in microns.
// Longitude pitch is 99.9 throughout; latitude pitch varies by zone.
const ZONES: [(f64, f64, f64, f64); 9] = [
    (0., 32., 369664., 99.9),
    (32., 48., 302592., 99.9),
    (48., 56., 245760., 100.),
    (56., 64., 199168., 99.9),
    (64., 68., 163328., 99.7),
    (68., 72., 137216., 99.7),
    (72., 76., 110080., 99.8),
    (76., 80., 82432., 100.),
    (80., 90., 400384., 99.9),
];

pub(super) struct Grid {
    zone: u8,
    origin: [f64; 2],
    step: [f64; 2],
    count: [u32; 2],
    resolution: [f64; 4],
}
impl Grid {
    pub(super) fn new(zone: u8, scale: u32) -> Result<Self> {
        if !(1..=18).contains(&zone) || !(1..=20_000_000).contains(&scale) {
            return Err(Error::InvalidArgument("CADRG zone must be 1..18 and reciprocal scale 1..20000000".into()));
        }
        let (min_lat, max_lat, a, pitch) = ZONES[usize::from((zone - 1) % 9)];
        let n = 1_000_000. / f64::from(scale);
        let lat_adrg = (400384. * n / 512.).ceil() * 512.;
        let lon_adrg = (a * n / 512.).ceil() * 512.;
        let lat = (lat_adrg / 1.5 / 4. / 256.).round() * 256.;
        let lon = (lon_adrg / 1.5 / 256.).round() * 256.;
        let mut resolution = [pitch / n * lat_adrg / (4. * lat), 99.9 / n * lon_adrg / lon, 90. / lat, 360. / lon];
        let (origin, step, count) = if zone.is_multiple_of(9) {
            let polar = (lat_adrg * 20. / 360. / 1.5 / 512.).round() * 512. * 360. / 20.;
            let step = RADIUS * 2. * std::f64::consts::PI / polar;
            let mut count = ((polar * 20. / 360. / 256.).round() / 6. - 1e-3).ceil() as u32;
            if count.is_multiple_of(2) {
                count += 1;
            }
            let origin = -f64::from(count) / 2. * step * FRAME;
            resolution[1] = resolution[0];
            resolution[3] = resolution[2];
            ([origin; 2], [step; 2], [count; 2])
        } else {
            let step = [resolution[3], resolution[2]];
            let low = (min_lat / (step[1] * FRAME)).floor() * (step[1] * FRAME);
            let high = (max_lat / (step[1] * FRAME)).ceil() * (step[1] * FRAME);
            let y = if zone > 9 { -high } else { low };
            let count = [(360. / (step[0] * FRAME) - 1e-3).ceil() as u32, ((high - low) / (step[1] * FRAME)).round() as u32];
            ([-180., y], step, count)
        };
        Ok(Self { zone, origin, step, count, resolution })
    }

    fn project(&self, lat: f64, lon: f64) -> [f64; 2] {
        if !self.zone.is_multiple_of(9) {
            return [lon, lat];
        }
        let north = self.zone == 9;
        let radius = RADIUS * (90. - if north { lat } else { -lat }).to_radians();
        let angle = lon.to_radians();
        [radius * angle.sin(), radius * angle.cos() * if north { -1. } else { 1. }]
    }
    fn geographic(&self, x: f64, y: f64) -> [f64; 2] {
        if !self.zone.is_multiple_of(9) {
            return [y, x];
        }
        let north = self.zone == 9;
        let radius = x.hypot(y);
        let lat = 90. - (radius / RADIUS).to_degrees();
        let lon = if radius == 0. { 0. } else { x.atan2(if north { -y } else { y }).to_degrees() };
        [if north { lat } else { -lat }, lon]
    }

    /// Geographic coverage and resolution for an inclusive frame rectangle.
    pub(super) fn coverage(&self, min: [u32; 2], max: [u32; 2]) -> [f64; 12] {
        let west = self.origin[0] + f64::from(min[0]) * self.step[0] * FRAME;
        let south = self.origin[1] + f64::from(min[1]) * self.step[1] * FRAME;
        let east = self.origin[0] + f64::from(max[0]) * self.step[0] * FRAME + self.step[0] * FRAME;
        let north = self.origin[1] + f64::from(max[1]) * self.step[1] * FRAME + self.step[1] * FRAME;
        let mut out = [0.; 12];
        for (to, [x, y]) in out[..8].as_chunks_mut::<2>().0.iter_mut().zip([[west, north], [west, south], [east, north], [east, south]]) {
            *to = self.geographic(x, y);
        }
        out[8..].copy_from_slice(&self.resolution);
        out
    }

    pub(super) fn locate(&self, frame: &RpfFrame) -> Result<[u32; 2]> {
        if frame.zone != self.zone || frame.coverage.iter().any(|v| !v.is_finite()) || frame.coverage[8..].iter().any(|v| *v <= 0.) {
            return Err(invalid("frame zone or coverage is invalid"));
        }
        // The SW corner determines a bottom-up grid index in both hemispheres.
        let sw = self.project(frame.coverage[2], frame.coverage[3]);
        let location = [(sw[0] - self.origin[0]) / (FRAME * self.step[0]), (sw[1] - self.origin[1]) / (FRAME * self.step[1])];
        let mut index = [0; 2];
        for i in 0..2 {
            if location[i] < -1e-3 || location[i].round() >= f64::from(self.count[i]) || (location[i] - location[i].round()).abs() > 1e-3 {
                return Err(invalid("frame coverage is not aligned with the selected CADRG scale/zone"));
            }
            index[i] = location[i].round() as u32;
        }
        let expected = self.coverage(index, index);
        // Allow at most 0.001 frame (1.536 pixels), as in the reference grid locator.
        // Compare projected corners to handle polar longitude discontinuities.
        for (a, b) in frame.coverage[..8].as_chunks::<2>().0.iter().zip(expected[..8].as_chunks::<2>().0) {
            if a[0].abs() > 90. || a[1].abs() > 360. {
                return Err(invalid("frame corner outside geographic bounds"));
            }
            let a = self.project(a[0], a[1]);
            let b = self.project(b[0], b[1]);
            if (0..2).any(|i| (a[i] - b[i]).abs() > FRAME * self.step[i] * 1e-3) {
                return Err(invalid("frame size or corners do not match the selected CADRG scale/zone"));
            }
        }
        // Historical polar frames carry unequal angular pitch/interval fields
        // even though their corners follow the standard spherical AEQD grid.
        // Derive output resolution from scale rather than rejecting those fields.
        if !self.zone.is_multiple_of(9) && frame.coverage[10..].iter().zip(&self.resolution[2..]).any(|(a, b)| (*a - b).abs() > b * 1e-6) {
            return Err(invalid("frame resolution does not match the selected CADRG scale/zone"));
        }
        Ok(index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn standard_grids_cover_all_zones_and_reject_wrong_scale_or_extent() {
        for zone in 1..=18 {
            let grid = Grid::new(zone, 1_000_000).unwrap();
            let at = [grid.count[0] / 2, grid.count[1] / 2];
            let mut frame =
                RpfFrame { name: String::new(), zone, series: "ON".into(), classification: 'U', coverage: grid.coverage(at, at) };
            assert_eq!(grid.locate(&frame).unwrap(), at);
            assert!(Grid::new(zone, 500_000).unwrap().locate(&frame).is_err());
            frame.coverage[0] += 0.1;
            assert!(grid.locate(&frame).is_err());
        }
        assert!(Grid::new(0, 1_000_000).is_err());
        assert!(Grid::new(19, 1_000_000).is_err());
        assert!(Grid::new(2, 0).is_err());
    }
    #[test]
    fn real_gdal_frames_match_the_standard_grid_when_available() {
        let Some(root) = std::env::var_os("TERRIO_TEST_DATA").map(std::path::PathBuf::from) else { return };
        for (name, scale, at) in [
            ("rpftoc_landsat/ON/RPF/ZONE2/000C9010.ON2", 1_000_000, [21, 3]),
            ("rpftoc_landsat/TP/RPF/ZONE2/001DP010.TP2", 500_000, [43, 6]),
            ("rpftoc_polar/RPF/ZONE9/00027010.ON9", 1_000_000, [9, 6]),
        ] {
            let path = root.join(name);
            if !path.is_file() {
                continue;
            }
            let frame = super::super::read_frame(name, &mut std::fs::File::open(path).unwrap()).unwrap();
            assert_eq!(Grid::new(frame.zone, scale).unwrap().locate(&frame).unwrap(), at);
        }
    }
}
