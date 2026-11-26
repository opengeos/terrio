// SPDX-FileCopyrightText: Copyright (c) 2023.
// SPDX-FileCopyrightText: Copyright (c) zune-image developers
// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
// Adapted from zune-jpegxl 0.5.2, src/errors.rs; see README.md in this directory.

#![allow(clippy::uninlined_format_args)]

use core::fmt::{Debug, Formatter};

use zune_core::bit_depth::BitDepth;
use zune_core::bytestream::ZByteIoError;
use zune_core::colorspace::ColorSpace;

const MAX_DIMENSIONS: usize = 1 << 30;

/// Errors that may arise during encoding
#[allow(dead_code)] // zune-jpegxl 0.5.2 never constructs two of them either.
pub enum JxlEncodeErrors {
    /// One of the dimensions is less than 2
    ZeroDimension(&'static str),
    /// The colorspace of the image isn't supported by
    /// the library
    UnsupportedColorspace(ColorSpace),
    /// Image depth isn't supported by the library
    UnsupportedDepth(BitDepth),
    /// A given width or height is too big to be encoded
    TooLargeDimensions(usize),
    /// Mismatch in length expected vs what was found
    LengthMismatch(usize, usize),
    /// Generic error
    Generic(&'static str),

    IoErrors(ZByteIoError),
}

pub const SUPPORTED_COLORSPACES: [ColorSpace; 4] = [ColorSpace::Luma, ColorSpace::LumaA, ColorSpace::RGBA, ColorSpace::RGB];
pub const SUPPORTED_DEPTHS: [BitDepth; 2] = [BitDepth::Eight, BitDepth::Sixteen];

impl Debug for JxlEncodeErrors {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        match self {
            JxlEncodeErrors::ZeroDimension(param) => write!(f, "The {param} is less than 2"),
            JxlEncodeErrors::UnsupportedColorspace(color) => {
                write!(f, "JXL encoder cannot encode images in colorspace {color:?}, supported ones are {:?}", SUPPORTED_COLORSPACES)
            }
            JxlEncodeErrors::UnsupportedDepth(depth) => {
                write!(f, "JXL encoder cannot encode images in depth {depth:?},supported ones are {:?}", SUPPORTED_DEPTHS)
            }
            JxlEncodeErrors::TooLargeDimensions(value) => {
                write!(f, "Too large dimensions {value} greater than supported dimensions {MAX_DIMENSIONS}")
            }
            JxlEncodeErrors::LengthMismatch(expected, found) => {
                write!(f, "Expected array of length {expected} but found {found}")
            }
            JxlEncodeErrors::Generic(msg) => {
                write!(f, "{}", msg)
            }
            JxlEncodeErrors::IoErrors(e) => {
                write!(f, "I/O error {:?}", e)
            }
        }
    }
}

impl From<ZByteIoError> for JxlEncodeErrors {
    fn from(value: ZByteIoError) -> Self {
        JxlEncodeErrors::IoErrors(value)
    }
}

impl core::fmt::Display for JxlEncodeErrors {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl std::error::Error for JxlEncodeErrors {}
