//! MD5 (RFC 1321), for comparing local files with the entity tags of S3 objects uploaded in one
//! request, which are their MD5 digests. Not for security.

/// An incremental MD5 digest.
pub(crate) struct Md5 {
    state: [u32; 4],
    buffer: Vec<u8>,
    length: u64,
}

const S: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4,
    11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

/// `floor(abs(sin(i + 1)) * 2^32)`.
const K: [u32; 64] = [
    0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501, 0x698098d8, 0x8b44f7af, 0xffff5bb1,
    0x895cd7be, 0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821, 0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d, 0x02441453,
    0xd8a1e681, 0xe7d3fbc8, 0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed, 0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a, 0xfffa3942,
    0x8771f681, 0x6d9d6122, 0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70, 0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05,
    0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665, 0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d,
    0x85845dd1, 0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb, 0xeb86d391,
];

impl Md5 {
    pub(crate) fn new() -> Self {
        Self { state: [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476], buffer: Vec::with_capacity(64), length: 0 }
    }

    pub(crate) fn update(&mut self, mut data: &[u8]) {
        self.length = self.length.wrapping_add(data.len() as u64);
        if !self.buffer.is_empty() {
            let take = (64 - self.buffer.len()).min(data.len());
            self.buffer.extend_from_slice(&data[..take]);
            data = &data[take..];
            if self.buffer.len() == 64 {
                let block: [u8; 64] = self.buffer[..].try_into().unwrap_or([0; 64]);
                self.block(&block);
                self.buffer.clear();
            }
        }
        let (blocks, rest) = data.as_chunks::<64>();
        for block in blocks {
            self.block(block);
        }
        self.buffer.extend_from_slice(rest);
    }

    pub(crate) fn finish(mut self) -> [u8; 16] {
        let bits = self.length.wrapping_mul(8);
        let mut pad = vec![0x80u8];
        let rem = (self.buffer.len() + 1) % 64;
        pad.resize(1 + if rem <= 56 { 56 - rem } else { 120 - rem }, 0);
        pad.extend_from_slice(&bits.to_le_bytes());
        let length = self.length;
        self.update(&pad);
        self.length = length;
        let mut out = [0u8; 16];
        for (i, w) in self.state.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
        }
        out
    }

    fn block(&mut self, block: &[u8; 64]) {
        let m: [u32; 16] = std::array::from_fn(|i| u32::from_le_bytes(block[4 * i..4 * i + 4].try_into().expect("four bytes")));
        let [mut a, mut b, mut c, mut d] = self.state;
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let f = f.wrapping_add(a).wrapping_add(K[i]).wrapping_add(m[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(f.rotate_left(S[i]));
        }
        for (s, v) in self.state.iter_mut().zip([a, b, c, d]) {
            *s = s.wrapping_add(v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vsi::md5_hex as hex_digest;

    /// The test suite of RFC 1321, appendix A.5.
    #[test]
    fn rfc1321_vectors() {
        assert_eq!(hex_digest(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(hex_digest(b"a"), "0cc175b9c0f1b6a831c399e269772661");
        assert_eq!(hex_digest(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(hex_digest(b"message digest"), "f96b697d7cb7938d525a2f31aaf161d0");
        assert_eq!(hex_digest(b"abcdefghijklmnopqrstuvwxyz"), "c3fcd3d76192e4007dfb496cca67e13b");
        assert_eq!(
            hex_digest(b"12345678901234567890123456789012345678901234567890123456789012345678901234567890"),
            "57edf4a22be3c955ac49da2e2107b67a"
        );
        // Fed in uneven pieces.
        let data: Vec<u8> = (0..1000u32).map(|i| (i * 7 % 251) as u8).collect();
        let mut h = Md5::new();
        for c in data.chunks(37) {
            h.update(c);
        }
        let pieces: String = h.finish().iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(pieces, hex_digest(&data));
    }
}
