// SPDX-FileCopyrightText: 2026 Qiusheng Wu
// SPDX-FileCopyrightText: 2026 T-Kartor
// SPDX-License-Identifier: MIT
//! MIL-C-89038 CADRG grids. Formula reference: GDAL 3.13.3 rpfframewriter.cpp (MIT).
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

    fn normalized_longitude(&self, longitude: f64) -> f64 {
        let mut delta = (longitude + 180.).rem_euclid(360.);
        // Keep tolerated roundoff just west of -180 on the first column.
        if delta > 360. - FRAME * self.step[0] * 1e-3 {
            delta = 0.;
        }
        delta - 180.
    }

    /// Minimum geographic latitude/longitude of the entire frame, including
    /// polar edges, longitude discontinuities and an interior pole.
    pub(super) fn geographic_min(&self, frame: &RpfFrame) -> [f64; 2] {
        let corners = frame.coverage[..8].as_chunks::<2>().0;
        let min_lon = corners.iter().map(|p| p[1]).fold(f64::INFINITY, f64::min);
        if !self.zone.is_multiple_of(9) {
            let west = self.normalized_longitude(frame.coverage[3]);
            let longitude = corners.iter().map(|p| west + (p[1] - west + 180.).rem_euclid(360.) - 180.).fold(f64::INFINITY, f64::min);
            return [corners.iter().map(|p| p[0]).fold(f64::INFINITY, f64::min), longitude];
        }
        let mut bounds = [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY];
        for p in corners {
            let [x, y] = self.project(p[0], p[1]);
            bounds[0] = bounds[0].min(x);
            bounds[1] = bounds[1].min(y);
            bounds[2] = bounds[2].max(x);
            bounds[3] = bounds[3].max(y);
        }
        let north = self.zone == 9;
        let radius = if north {
            bounds[0].abs().max(bounds[2].abs()).hypot(bounds[1].abs().max(bounds[3].abs()))
        } else {
            0f64.clamp(bounds[0], bounds[2]).hypot(0f64.clamp(bounds[1], bounds[3]))
        };
        let min_lat = if north { 90. - (radius / RADIUS).to_degrees() } else { -90. + (radius / RADIUS).to_degrees() };
        let crosses_dateline = bounds[0] <= 0. && bounds[2] >= 0. && if north { bounds[3] > 0. } else { bounds[1] < 0. };
        [min_lat, if crosses_dateline { -180. } else { min_lon }]
    }

    pub(super) fn locate(&self, frame: &RpfFrame) -> Result<[u32; 2]> {
        if frame.zone != self.zone || frame.coverage.iter().any(|v| !v.is_finite()) || frame.coverage[8..].iter().any(|v| *v <= 0.) {
            return Err(invalid("frame zone or coverage is invalid"));
        }
        // The SW corner determines a bottom-up grid index in both hemispheres.
        let mut sw = self.project(frame.coverage[2], frame.coverage[3]);
        if !self.zone.is_multiple_of(9) {
            sw[0] = self.normalized_longitude(sw[0]);
        }
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
            let longitude_error =
                if self.zone.is_multiple_of(9) { (a[0] - b[0]).abs() } else { ((a[0] - b[0] + 180.).rem_euclid(360.) - 180.).abs() };
            if longitude_error > FRAME * self.step[0] * 1e-3 || (a[1] - b[1]).abs() > FRAME * self.step[1] * 1e-3 {
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
    #[ignore = "requires prepared RPF fixtures; see docs/testing.md"]
    fn real_gdal_frames_match_the_standard_grid_when_available() {
        let root = std::path::PathBuf::from(std::env::var_os("TERRIO_TEST_DATA").expect("set TERRIO_TEST_DATA to prepared RPF fixtures"));
        let mut paths = super::super::prepared_flat_frames(&root);
        paths.push(root.join("rpftoc_polar/RPF/ZONE9/00027010.ON9"));
        for path in paths {
            assert!(path.is_file(), "missing RPF fixture {}", path.display());
            let name = path.strip_prefix(&root).unwrap().to_str().unwrap().replace(std::path::MAIN_SEPARATOR, "/");
            let frame = super::super::read_frame(&name, &mut std::fs::File::open(&path).unwrap()).unwrap();
            let scale = frame.inferred_scale().unwrap();
            // Independent GDAL naming oracle: base-34 frame number, with the
            // known 1M/500K zone-2 and 1M polar column counts.
            let alphabet = b"0123456789ABCDEFGHJKLMNPQRSTUVWXYZ";
            let number = path.file_name().unwrap().to_str().unwrap()[..5]
                .bytes()
                .fold(0u32, |n, c| n * 34 + alphabet.iter().position(|v| *v == c).unwrap() as u32);
            let columns = if frame.zone == 9 {
                11
            } else if scale == 500_000 {
                263
            } else {
                132
            };
            assert_eq!(Grid::new(frame.zone, scale).unwrap().locate(&frame).unwrap(), [number % columns, number / columns]);
        }
    }
}
