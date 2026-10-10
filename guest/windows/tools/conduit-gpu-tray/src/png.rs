//! A tiny PNG encoder (8-bit RGBA, zlib "stored" blocks): enough for the
//! 64 px app icons, with no compression code to get wrong.

fn crc_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    for (n, slot) in t.iter_mut().enumerate() {
        let mut c = n as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
        *slot = c;
    }
    t
}

/// CRC-32 (IEEE), as PNG chunks use it.
pub fn crc32(parts: &[&[u8]]) -> u32 {
    let t = crc_table();
    let mut c = 0xFFFF_FFFFu32;
    for p in parts {
        for &b in *p {
            c = t[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
        }
    }
    c ^ 0xFFFF_FFFF
}

/// Adler-32, the zlib trailer.
pub fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for chunk in data.chunks(5552) {
        for &x in chunk {
            a += x as u32;
            b += a;
        }
        a %= 65521;
        b %= 65521;
    }
    (b << 16) | a
}

/// `data` as a zlib stream of stored (uncompressed) deflate blocks.
pub fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    let mut blocks = data.chunks(65535).peekable();
    if blocks.peek().is_none() {
        out.extend_from_slice(&[1, 0, 0, 0xFF, 0xFF]);
    }
    while let Some(b) = blocks.next() {
        out.push(u8::from(blocks.peek().is_none()));
        let n = b.len() as u16;
        out.extend_from_slice(&n.to_le_bytes());
        out.extend_from_slice(&(!n).to_le_bytes());
        out.extend_from_slice(b);
    }
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    out.extend_from_slice(&crc32(&[kind, body]).to_be_bytes());
}

/// Encodes `w` x `h` pixels, `rgba` four bytes each (not premultiplied).
/// `None` when the buffer does not match the size.
pub fn encode_rgba(w: u32, h: u32, rgba: &[u8]) -> Option<Vec<u8>> {
    let row = (w as usize).checked_mul(4)?;
    if w == 0 || h == 0 || rgba.len() != row.checked_mul(h as usize)? {
        return None;
    }
    let mut raw = Vec::with_capacity((row + 1) * h as usize);
    for line in rgba.chunks_exact(row) {
        raw.push(0); // filter: none
        raw.extend_from_slice(line);
    }
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit RGBA, no interlace
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &zlib_stored(&raw));
    chunk(&mut out, b"IEND", &[]);
    Some(out)
}

/// The smallest rectangle holding every pixel that is not (nearly)
/// transparent: `(x, y, w, h)`. `None` for an empty image.
pub fn alpha_bounds(rgba: &[u8], w: u32, h: u32) -> Option<(u32, u32, u32, u32)> {
    let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0u32, 0u32);
    for y in 0..h {
        for x in 0..w {
            let a = rgba.get((y as usize * w as usize + x as usize) * 4 + 3)?;
            if *a > 8 {
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x);
                y1 = y1.max(y);
            }
        }
    }
    (x1 >= x0 && y1 >= y0 && x0 < w).then(|| (x0, y0, x1 - x0 + 1, y1 - y0 + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksums_match_known_values() {
        assert_eq!(crc32(&[b"123456789"]), 0xCBF4_3926);
        assert_eq!(crc32(&[b"1234", b"56789"]), 0xCBF4_3926);
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
        assert_eq!(adler32(b""), 1);
    }

    /// Reads back the stored blocks of a zlib stream.
    fn inflate_stored(z: &[u8]) -> Vec<u8> {
        assert_eq!(&z[..2], &[0x78, 0x01]);
        let mut i = 2;
        let mut out = Vec::new();
        loop {
            let last = z[i] & 1 == 1;
            assert_eq!(z[i] & 0xFE, 0, "stored block");
            let n = u16::from_le_bytes([z[i + 1], z[i + 2]]);
            let nn = u16::from_le_bytes([z[i + 3], z[i + 4]]);
            assert_eq!(n, !nn);
            out.extend_from_slice(&z[i + 5..i + 5 + n as usize]);
            i += 5 + n as usize;
            if last {
                break;
            }
        }
        assert_eq!(&z[i..], &adler32(&out).to_be_bytes());
        out
    }

    #[test]
    fn zlib_round_trips_across_block_sizes() {
        for n in [0usize, 1, 65534, 65535, 65536, 140_000] {
            let data: Vec<u8> = (0..n).map(|i| (i * 7 % 251) as u8).collect();
            assert_eq!(inflate_stored(&zlib_stored(&data)), data, "{n}");
        }
    }

    /// Splits a PNG into (kind, body) after checking signature and CRCs.
    fn chunks(png: &[u8]) -> Vec<([u8; 4], Vec<u8>)> {
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        let mut i = 8;
        let mut v = Vec::new();
        while i < png.len() {
            let n = u32::from_be_bytes(png[i..i + 4].try_into().unwrap()) as usize;
            let kind: [u8; 4] = png[i + 4..i + 8].try_into().unwrap();
            let body = png[i + 8..i + 8 + n].to_vec();
            let crc = u32::from_be_bytes(png[i + 8 + n..i + 12 + n].try_into().unwrap());
            assert_eq!(crc, crc32(&[&kind, &body]), "crc of {:?}", kind);
            v.push((kind, body));
            i += 12 + n;
        }
        v
    }

    #[test]
    fn encodes_a_two_by_two_image() {
        // red, green / blue, half-transparent white
        let px: [u8; 16] = [
            255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 128,
        ];
        let png = encode_rgba(2, 2, &px).unwrap();
        let c = chunks(&png);
        assert_eq!(c.len(), 3);
        assert_eq!(&c[0].0, b"IHDR");
        assert_eq!(c[0].1, [0, 0, 0, 2, 0, 0, 0, 2, 8, 6, 0, 0, 0]);
        assert_eq!(&c[1].0, b"IDAT");
        assert_eq!(&c[2].0, b"IEND");
        assert!(c[2].1.is_empty());
        let raw = inflate_stored(&c[1].1);
        assert_eq!(raw.len(), 2 * (1 + 8));
        assert_eq!(raw[0], 0);
        assert_eq!(&raw[1..9], &px[..8]);
        assert_eq!(raw[9], 0);
        assert_eq!(&raw[10..], &px[8..]);
    }

    #[test]
    fn large_image_spans_blocks_and_bad_sizes_are_refused() {
        let px = vec![200u8; 64 * 64 * 4];
        let png = encode_rgba(64, 64, &px).unwrap();
        let c = chunks(&png);
        assert_eq!(inflate_stored(&c[1].1).len(), 64 * (64 * 4 + 1));
        assert!(encode_rgba(2, 2, &px[..15]).is_none());
        assert!(encode_rgba(0, 2, &[]).is_none());
    }

    #[test]
    fn alpha_bounds_finds_the_content() {
        let mut px = vec![0u8; 8 * 6 * 4];
        assert_eq!(alpha_bounds(&px, 8, 6), None);
        for (x, y) in [(2u32, 1u32), (5, 4)] {
            px[(y as usize * 8 + x as usize) * 4 + 3] = 255;
        }
        assert_eq!(alpha_bounds(&px, 8, 6), Some((2, 1, 4, 4)));
        assert_eq!(alpha_bounds(&px[..10], 8, 6), None);
    }
}
