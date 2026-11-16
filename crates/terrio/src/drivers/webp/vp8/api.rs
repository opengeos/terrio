// SPDX-FileCopyrightText: Copyright (c) 2026 MITH@mmk
// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
// Adapted from webp-rust 0.3.2, src/encoder/; see README.md in this directory.

//! Public entry points and final frame assembly for lossy encoding.

use super::bitstream::*;
use super::predict::*;
use super::*;

/// Builds a raw VP8 frame from the already encoded partitions.
fn build_vp8_frame(width: usize, height: usize, partition0: &[u8], token_partition: &[u8]) -> Result<Vec<u8>, EncoderError> {
    if partition0.len() > MAX_PARTITION0_LENGTH {
        return Err(EncoderError::Bitstream("VP8 partition 0 overflow"));
    }

    let payload_size = 10usize
        .checked_add(partition0.len())
        .and_then(|size| size.checked_add(token_partition.len()))
        .ok_or(EncoderError::InvalidParam("encoded output is too large"))?;

    let mut data = Vec::with_capacity(payload_size);
    let frame_bits = ((partition0.len() as u32) << 5) | (1 << 4);
    data.extend_from_slice(&frame_bits.to_le_bytes()[..3]);
    data.extend_from_slice(&[0x9d, 0x01, 0x2a]);
    data.extend_from_slice(&(width as u16).to_le_bytes());
    data.extend_from_slice(&(height as u16).to_le_bytes());
    data.extend_from_slice(partition0);
    data.extend_from_slice(token_partition);
    Ok(data)
}

/// Builds a VP8 frame for a candidate mode/filter combination.
fn build_candidate_vp8_frame(
    width: usize,
    height: usize,
    mb_width: usize,
    mb_height: usize,
    candidate: &EncodedLossyCandidate,
    filter: &FilterConfig,
) -> Result<Vec<u8>, EncoderError> {
    let segment = segment_with_uniform_filter(&candidate.segment, filter.level);
    let partition0 =
        encode_partition0(mb_width, mb_height, candidate.base_quant, &segment, filter, &candidate.probabilities, &candidate.modes);
    build_vp8_frame(width, height, &partition0, &candidate.token_partition)
}

/// Encodes one lossy candidate and captures its token partition, probabilities, and modes.
fn encode_lossy_candidate(
    source: &Planes,
    mb_width: usize,
    mb_height: usize,
    profile: &LossySearchProfile,
    segment: &SegmentConfig,
) -> Result<EncodedLossyCandidate, EncoderError> {
    let segment_quants = build_segment_quantizers(segment);
    let (token_partition, probabilities, modes) = if profile.update_probabilities {
        let mut stats = [[[[0u32; NUM_PROBAS]; NUM_CTX]; NUM_BANDS]; NUM_TYPES];
        let (initial_partition, _, initial_modes) =
            encode_token_partition(source, mb_width, mb_height, profile, segment, &segment_quants, &COEFFS_PROBA0, Some(&mut stats));
        let probabilities = finalize_token_probabilities(&stats);
        if probabilities == COEFFS_PROBA0 {
            (initial_partition, probabilities, initial_modes)
        } else {
            let (token_partition, _, modes) =
                encode_token_partition(source, mb_width, mb_height, profile, segment, &segment_quants, &probabilities, None);
            (token_partition, probabilities, modes)
        }
    } else {
        let (token_partition, _, modes) =
            encode_token_partition(source, mb_width, mb_height, profile, segment, &segment_quants, &COEFFS_PROBA0, None);
        (token_partition, COEFFS_PROBA0, modes)
    };
    Ok(EncodedLossyCandidate { base_quant: segment.quantizer[0], segment: segment.clone(), probabilities, modes, token_partition })
}

/// Finalizes the lossy candidate by choosing the best filter configuration.
#[allow(clippy::too_many_arguments)]
fn finalize_lossy_candidate(
    width: usize,
    height: usize,
    source: &Planes,
    mb_width: usize,
    mb_height: usize,
    base_quant: i32,
    config: &LossyEncodingConfig,
    candidate: &EncodedLossyCandidate,
) -> Result<Vec<u8>, EncoderError> {
    let mb_count = mb_width * mb_height;
    let configured_level = ((config.filter_strength as u16 * 63 + 50) / 100) as u8;
    let configured_filter = FilterConfig { simple: !config.strong_filter, level: configured_level, sharpness: config.filter_sharpness };
    if !use_exhaustive_filter_search(config.method, mb_count) {
        let filter = configured_filter;
        return build_candidate_vp8_frame(width, height, mb_width, mb_height, candidate, &filter);
    }

    let filters = filter_candidates(base_quant)
        .into_iter()
        .map(|filter| FilterConfig {
            simple: configured_filter.simple,
            level: ((filter.level as u16 * configured_level.max(1) as u16 + 31) / 63) as u8,
            sharpness: configured_filter.sharpness,
        })
        .collect::<Vec<_>>();
    let mut best = None;
    for filter in &filters {
        let vp8 = build_candidate_vp8_frame(width, height, mb_width, mb_height, candidate, filter)?;
        let distortion = yuv_sse(source, width, height, &vp8)?;
        let replace = match &best {
            Some((best_distortion, best_len, _)) => {
                distortion < *best_distortion || (distortion == *best_distortion && vp8.len() < *best_len)
            }
            None => true,
        };
        if replace {
            best = Some((distortion, vp8.len(), vp8));
        }
    }

    best.map(|(_, _, vp8)| vp8).ok_or(EncoderError::Bitstream("lossy filter search produced no output"))
}

fn prepare_lossy_color(rgba: &[u8], exact: bool, _no_alpha: bool) -> Vec<u8> {
    let mut color = rgba.to_vec();
    for pixel in color.as_chunks_mut::<4>().0 {
        if !exact && pixel[3] == 0 {
            pixel[..3].fill(0);
        }
        pixel[3] = 0xff;
    }
    color
}

fn apply_near_lossless(rgba: &mut [u8], near_lossless: u8) {
    if near_lossless >= 100 {
        return;
    }
    let step = ((100 - near_lossless) / 8).max(1);
    for pixel in rgba.as_chunks_mut::<4>().0 {
        for channel in &mut pixel[..3] {
            let value = *channel as u16;
            *channel = ((value / step as u16) * step as u16).min(255) as u8;
        }
    }
}

/// Encodes RGBA pixels to a raw lossy `VP8` frame payload with libwebp-style options.
pub fn encode_lossy_rgba_to_vp8_with_config(
    width: usize,
    height: usize,
    rgba: &[u8],
    config: &LossyEncodingConfig,
) -> Result<Vec<u8>, EncoderError> {
    validate_rgba(width, height, rgba)?;
    validate_config(config)?;

    let mut color = prepare_lossy_color(rgba, config.exact, config.no_alpha);
    apply_near_lossless(&mut color, config.near_lossless);

    let mb_width = (width + 15) >> 4;
    let mb_height = (height + 15) >> 4;
    let base_quant = base_quantizer_from_quality(config.quality).saturating_add((config.partition_limit / 20) as i32);
    let profile = lossy_search_profile(config.method);
    let source = rgba_to_yuv420_with_sharp_yuv(width, height, &color, mb_width, mb_height, config.sharp_yuv);
    let candidates =
        build_segment_candidates(&source, mb_width, mb_height, base_quant, config.method, config.segments, config.sns_strength);
    let mut best = None;
    for segment in &candidates {
        let candidate = encode_lossy_candidate(&source, mb_width, mb_height, &profile, segment)?;
        let vp8 = finalize_lossy_candidate(width, height, &source, mb_width, mb_height, base_quant, config, &candidate)?;
        let replace = match &best {
            Some((best_bytes, _)) => vp8.len() < *best_bytes,
            None => true,
        };
        if replace {
            best = Some((vp8.len(), vp8));
        }
    }

    best.map(|(_, vp8)| vp8).ok_or(EncoderError::Bitstream("lossy candidate search produced no output"))
}
