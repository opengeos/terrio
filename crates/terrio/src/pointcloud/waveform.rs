//! Full waveforms (LAS 1.3 and 1.4 point formats 4, 5, 9 and 10).
//!
//! Each point refers to its waveform with its `wavepacket_index` (the wave packet descriptor
//! that describes the samples: `LASF_Spec` VLR 99 + index), `wavepacket_offset` (where the samples
//! are, from the start of the waveform data packet record, its 60-byte header included) and
//! `wavepacket_size` (their size in bytes). The packets are in the file (global encoding bit 1:
//! the waveform data packet record, an EVLR) or in a `.wdp` file next to it (bit 2), as LASlib
//! reads them.

use super::las::{LASF_SPEC, LasHeader};
use crate::{Error, Result};
use serde::Serialize;

/// A wave packet descriptor (`LASF_Spec` VLRs 100 to 354).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct WavePacketDescriptor {
    /// The index the points refer to it by (1 to 255): its record id minus 99.
    pub index: u8,
    /// The bits per sample (8 or 16, usually).
    pub bits_per_sample: u8,
    /// The waveform compression type (0: none).
    pub compression_type: u8,
    /// The number of samples.
    pub number_of_samples: u32,
    /// The time between samples, in picoseconds.
    pub temporal_sample_spacing: u32,
    /// The digitizer gain: volts = offset + gain × sample.
    pub digitizer_gain: f64,
    /// The digitizer offset.
    pub digitizer_offset: f64,
}

impl WavePacketDescriptor {
    /// The descriptors of a header, by index.
    pub fn from_header(h: &LasHeader) -> Vec<Self> {
        let mut out: Vec<Self> = h
            .all_vlrs()
            .filter(|v| v.user_id == LASF_SPEC && (100..=354).contains(&v.record_id) && v.data.len() >= 26)
            .map(|v| {
                let b = &v.data;
                let u32_at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap_or_default());
                let f64_at = |o: usize| f64::from_le_bytes(b[o..o + 8].try_into().unwrap_or_default());
                Self {
                    index: (v.record_id - 99).min(255) as u8,
                    bits_per_sample: b[0],
                    compression_type: b[1],
                    number_of_samples: u32_at(2),
                    temporal_sample_spacing: u32_at(6),
                    digitizer_gain: f64_at(10),
                    digitizer_offset: f64_at(18),
                }
            })
            .collect();
        out.sort_by_key(|d| d.index);
        out
    }
}

/// A point's waveform.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Waveform {
    /// The descriptor of its samples.
    pub descriptor: WavePacketDescriptor,
    /// The samples, as digitized.
    pub samples: Vec<u32>,
}

impl Waveform {
    /// Decodes the samples of a packet (8, 16 or 32 bits per sample, little-endian, or any other
    /// number of bits, packed from the least significant bit).
    pub fn decode(descriptor: WavePacketDescriptor, bytes: &[u8]) -> Result<Self> {
        let bits = descriptor.bits_per_sample as usize;
        if bits == 0 || bits > 32 {
            return Err(Error::InvalidData(format!("{bits} bits per waveform sample")));
        }
        if descriptor.compression_type != 0 {
            return Err(Error::Unsupported(format!("waveform compression type {}", descriptor.compression_type)));
        }
        let n = (descriptor.number_of_samples as usize).min(bytes.len() * 8 / bits);
        let samples = match bits {
            8 => bytes[..n].iter().map(|b| *b as u32).collect(),
            16 => bytes.as_chunks::<2>().0.iter().take(n).map(|c| u16::from_le_bytes(*c) as u32).collect(),
            32 => bytes.as_chunks::<4>().0.iter().take(n).map(|c| u32::from_le_bytes(*c)).collect(),
            _ => (0..n)
                .map(|i| {
                    let mut v = 0u64;
                    for k in 0..bits {
                        let bit = i * bits + k;
                        v |= (((bytes[bit / 8] >> (bit % 8)) & 1) as u64) << k;
                    }
                    v as u32
                })
                .collect(),
        };
        Ok(Self { descriptor, samples })
    }

    /// The samples in volts: `offset + gain × sample`.
    pub fn volts(&self) -> Vec<f64> {
        self.samples.iter().map(|s| self.descriptor.digitizer_offset + self.descriptor.digitizer_gain * *s as f64).collect()
    }

    /// The time of each sample after the first, in picoseconds.
    pub fn times(&self) -> Vec<f64> {
        (0..self.samples.len()).map(|i| i as f64 * self.descriptor.temporal_sample_spacing as f64).collect()
    }
}
