// SPDX-FileCopyrightText: Copyright (c) 2023.
// SPDX-FileCopyrightText: Copyright (c) zune-image developers
// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
// Adapted from zune-jpegxl 0.5.2, src/lib.rs; see README.md in this directory.

//! The lossless JPEG XL encoder of zune-jpegxl 0.5.2, with its long-run code built in 64 bits and
//! room for the widest 16-bit codes.

#![forbid(unsafe_code)]
// The snapshot keeps zune-jpegxl's code as written, which newer Clippy lints flag.
#![allow(clippy::chunks_exact_to_as_chunks, clippy::doc_lazy_continuation, clippy::manual_div_ceil, clippy::manual_is_multiple_of)]

mod bit_depth;
mod bit_writer;
mod color_convert;
mod encoder;
mod errors;

pub(crate) use encoder::JxlSimpleEncoder;
use errors::JxlEncodeErrors;
