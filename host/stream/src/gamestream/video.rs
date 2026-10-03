//! GameStream video packets.
//!
//! An encoded frame gets an 8-byte frame header, is cut into shards of
//! `packet_size - 16` payload bytes, and each shard is prefixed with
//!
//!   RTP (12, big-endian) | 4 reserved | NV_VIDEO_PACKET (16, little-endian)
//!
//! so every datagram is `packet_size + 16` bytes. Up to 4 FEC blocks per
//! frame; each block gets Reed-Solomon parity shards over the whole datagram
//! (headers included, as they were when the parity was computed — the client
//! patches the RTP/NV fields of a recovered shard itself). Optionally each
//! datagram is AES-GCM encrypted behind a 32-byte prefix.

use openssl::symm::{encrypt_aead, Cipher};

pub const RTP_HEADER: usize = 12;
pub const MAX_RTP_HEADER: usize = 16;
pub const NV_HEADER: usize = 16;
pub const SHARD_HEADER: usize = RTP_HEADER + 4 + NV_HEADER; // 32
pub const FRAME_HEADER: usize = 8;
pub const ENC_PREFIX: usize = 32;
pub const DATA_SHARDS_MAX: usize = 255;
pub const MAX_FEC_BLOCKS: usize = 4;

const FLAG_CONTAINS_PIC_DATA: u8 = 0x1;
const FLAG_EOF: u8 = 0x2;
const FLAG_SOF: u8 = 0x4;
const RTP_FLAG_EXTENSION: u8 = 0x10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameType {
    P = 1,
    Idr = 2,
    AfterRfi = 5,
}

extern "C" {
    fn cs_rs_encode(ds: i32, ps: i32, shards: *mut *mut u8, bs: i32, parity: *const u8) -> i32;
    #[cfg(test)]
    fn cs_rs_decode(ds: i32, ps: i32, shards: *mut *mut u8, marks: *mut u8, bs: i32) -> i32;
}

/// Reed-Solomon over `shards` (data first, then parity to be filled).
pub fn rs_encode(shards: &mut [Vec<u8>], data: usize, parity_override: Option<&[u8]>) -> bool {
    let parity = shards.len() - data;
    if parity == 0 {
        return true;
    }
    let bs = shards[0].len();
    let mut ptrs: Vec<*mut u8> = shards.iter_mut().map(|s| s.as_mut_ptr()).collect();
    // SAFETY: every shard is `bs` bytes and outlives the call; the matrix
    // override (if any) is data*parity bytes.
    unsafe {
        cs_rs_encode(
            data as i32,
            parity as i32,
            ptrs.as_mut_ptr(),
            bs as i32,
            parity_override.map_or(std::ptr::null(), |p| p.as_ptr()),
        ) == 0
    }
}

pub struct Packetizer {
    pub packet_size: usize,
    pub fec_percent: usize,
    pub min_fec: usize,
    /// AES-GCM key when video encryption is on.
    pub key: Option<[u8; 16]>,
    /// Packets sent so far: RTP takes the low 16 bits, the NV header's
    /// streamPacketIndex 24 (the client checks continuity on 24).
    seq: u32,
    iv_counter: u64,
}

impl Packetizer {
    pub fn new(
        packet_size: usize,
        fec_percent: usize,
        min_fec: usize,
        key: Option<[u8; 16]>,
    ) -> Self {
        Packetizer {
            packet_size,
            fec_percent,
            min_fec,
            key,
            seq: 0,
            iv_counter: 0,
        }
    }

    /// Datagram size on the wire.
    pub fn block(&self) -> usize {
        self.packet_size + MAX_RTP_HEADER
    }

    /// All datagrams of one frame, in send order.
    pub fn frame(
        &mut self,
        data: &[u8],
        frame_index: u32,
        ftype: FrameType,
        latency_tenth_ms: u16,
        rtp_ts: u32,
    ) -> Vec<Vec<u8>> {
        let block = self.block();
        let payload_per = block - SHARD_HEADER;
        // frame header
        let mut hdr = [0u8; FRAME_HEADER];
        hdr[0] = 0x01;
        hdr[1..3].copy_from_slice(&latency_tenth_ms.to_le_bytes());
        hdr[3] = ftype as u8;
        let total = data.len() + FRAME_HEADER;
        let mut last = (total % (self.packet_size - NV_HEADER)) as u16;
        if last == 0 {
            last = (self.packet_size - NV_HEADER) as u16;
        }
        hdr[4..6].copy_from_slice(&last.to_le_bytes());

        // Shards with room for the headers; payload = frame header + data.
        let nshards = total.div_ceil(payload_per);
        let mut shards: Vec<Vec<u8>> = Vec::with_capacity(nshards);
        let mut src = hdr.iter().chain(data.iter()).copied();
        for _ in 0..nshards {
            let mut s = vec![0u8; block];
            for b in s[SHARD_HEADER..].iter_mut() {
                match src.next() {
                    Some(v) => *b = v,
                    None => break,
                }
            }
            shards.push(s);
        }

        // FEC blocks: as few as fit DATA_SHARDS_MAX shards with parity.
        let mut fec = self.fec_percent;
        let max_data = (DATA_SHARDS_MAX * 100) / (100 + fec);
        let mut nblocks = nshards.div_ceil(max_data.max(1)).max(1);
        if nblocks > MAX_FEC_BLOCKS {
            fec = 0;
            nblocks = MAX_FEC_BLOCKS;
        }
        let per_block = nshards.div_ceil(nblocks);
        let mut out = Vec::new();
        let mut it = shards.into_iter();
        for bi in 0..nblocks {
            let mut blk: Vec<Vec<u8>> = it
                .by_ref()
                .take(if bi + 1 == nblocks {
                    usize::MAX
                } else {
                    per_block
                })
                .collect();
            let data_n = blk.len();
            if data_n == 0 {
                continue;
            }
            let multi = ((bi as u8) << 4) | (((nblocks - 1) as u8) << 6);
            for (x, s) in blk.iter_mut().enumerate() {
                let nv = &mut s[RTP_HEADER + 4..SHARD_HEADER];
                nv[0..4].copy_from_slice(
                    &(((self.seq as u32).wrapping_add(x as u32)) << 8).to_le_bytes(),
                );
                nv[4..8].copy_from_slice(&frame_index.to_le_bytes());
                let mut flags = FLAG_CONTAINS_PIC_DATA;
                if x == 0 {
                    flags |= FLAG_SOF;
                }
                if x + 1 == data_n {
                    flags |= FLAG_EOF;
                }
                nv[8] = flags;
                nv[10] = 0x10;
                nv[11] = multi;
            }
            let mut parity_n = if fec == 0 {
                0
            } else {
                (data_n * fec).div_ceil(100)
            };
            let mut pct = fec;
            if parity_n < self.min_fec && fec != 0 {
                parity_n = self.min_fec;
                pct = 100 * parity_n / data_n;
            }
            if data_n + parity_n > DATA_SHARDS_MAX {
                parity_n = DATA_SHARDS_MAX.saturating_sub(data_n);
            }
            for _ in 0..parity_n {
                blk.push(vec![0u8; block]);
            }
            if parity_n > 0 && !rs_encode(&mut blk, data_n, None) {
                blk.truncate(data_n);
                pct = 0;
            }
            let n = blk.len();
            for (x, s) in blk.iter_mut().enumerate() {
                let seq = self.seq.wrapping_add(x as u32) as u16;
                s[0] = 0x80 | RTP_FLAG_EXTENSION;
                s[1] = 0;
                s[2..4].copy_from_slice(&seq.to_be_bytes());
                s[4..8].copy_from_slice(&rtp_ts.to_be_bytes());
                s[8..12].copy_from_slice(&0u32.to_be_bytes());
                let nv = &mut s[RTP_HEADER + 4..SHARD_HEADER];
                let fec_info =
                    ((x as u32) << 12) | ((data_n as u32) << 22) | ((pct as u32 & 0xff) << 4);
                nv[12..16].copy_from_slice(&fec_info.to_le_bytes());
                nv[10] = 0x10;
                nv[11] = multi;
                nv[4..8].copy_from_slice(&frame_index.to_le_bytes());
            }
            self.seq = self.seq.wrapping_add(n as u32);
            for s in blk {
                out.push(match self.key {
                    Some(k) => self.encrypt(&k, s, frame_index),
                    None => s,
                });
            }
        }
        out
    }

    fn encrypt(&mut self, key: &[u8; 16], s: Vec<u8>, frame_index: u32) -> Vec<u8> {
        let mut iv = [0u8; 12];
        iv[..8].copy_from_slice(&self.iv_counter.to_le_bytes());
        iv[11] = b'V';
        self.iv_counter += 1;
        let mut tag = [0u8; 16];
        let ct = encrypt_aead(Cipher::aes_128_gcm(), key, Some(&iv), &[], &s, &mut tag)
            .expect("AES-GCM");
        let mut o = Vec::with_capacity(ENC_PREFIX + ct.len());
        o.extend_from_slice(&iv);
        o.extend_from_slice(&frame_index.to_le_bytes());
        o.extend_from_slice(&tag);
        o.extend_from_slice(&ct);
        o
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rs_decode(shards: &mut [Vec<u8>], data: usize, lost: &[usize]) -> bool {
        let mut marks = vec![0u8; shards.len()];
        for &l in lost {
            marks[l] = 1;
            shards[l].iter_mut().for_each(|b| *b = 0);
        }
        let bs = shards[0].len();
        let mut ptrs: Vec<*mut u8> = shards.iter_mut().map(|s| s.as_mut_ptr()).collect();
        unsafe {
            cs_rs_decode(
                data as i32,
                (shards.len() - data) as i32,
                ptrs.as_mut_ptr(),
                marks.as_mut_ptr(),
                bs as i32,
            ) == 0
        }
    }

    fn nv(s: &[u8]) -> (u32, u32, u8, u8, u8, u32) {
        let n = &s[16..32];
        (
            u32::from_le_bytes(n[0..4].try_into().unwrap()),
            u32::from_le_bytes(n[4..8].try_into().unwrap()),
            n[8],
            n[10],
            n[11],
            u32::from_le_bytes(n[12..16].try_into().unwrap()),
        )
    }

    #[test]
    fn small_frame_layout() {
        let mut p = Packetizer::new(1024, 20, 2, None);
        let data: Vec<u8> = (0..3000u32).map(|i| i as u8).collect();
        let pk = p.frame(&data, 1, FrameType::Idr, 12, 9000);
        // (3000+8)/(1040-32)=2.98 -> 3 data + max(ceil(0.6),2)=2 parity
        assert_eq!(pk.len(), 5);
        for (i, s) in pk.iter().enumerate() {
            assert_eq!(s.len(), 1040);
            assert_eq!(s[0], 0x90);
            assert_eq!(u16::from_be_bytes([s[2], s[3]]), i as u16);
            assert_eq!(u32::from_be_bytes(s[4..8].try_into().unwrap()), 9000);
            let (spi, fi, flags, mff, mfb, fec) = nv(s);
            assert_eq!(fi, 1);
            assert_eq!(mff, 0x10);
            assert_eq!(mfb, 0);
            assert_eq!((fec >> 12) & 0x3ff, i as u32);
            assert_eq!(fec >> 22, 3);
            assert_eq!((fec >> 4) & 0xff, 66); // 100*2/3
            if i < 3 {
                assert_eq!(spi, (i as u32) << 8);
                assert_eq!(flags & 1, 1);
            }
        }
        let first = &pk[0][32..];
        assert_eq!(first[0], 1); // short header
        assert_eq!(first[3], 2); // IDR
        assert_eq!(u16::from_le_bytes([first[1], first[2]]), 12);
        let last = u16::from_le_bytes([first[4], first[5]]) as usize;
        assert_eq!(last, (3000 + 8) % (1024 - 16));
        assert_eq!(&first[8..18], &data[..10]);
        assert_eq!(nv(&pk[0]).2, 0x5); // SOF | data
        assert_eq!(nv(&pk[2]).2, 0x3); // EOF | data
                                       // sequence numbers continue into the next frame
        let pk2 = p.frame(&data[..10], 2, FrameType::P, 0, 9375);
        assert_eq!(u16::from_be_bytes([pk2[0][2], pk2[0][3]]), 5);
    }

    #[test]
    fn parity_recovers_lost_shards_as_the_client_would() {
        let mut p = Packetizer::new(1392, 20, 2, None);
        let data: Vec<u8> = (0..50_000u32).map(|i| (i * 7) as u8).collect();
        let pk = p.frame(&data, 7, FrameType::P, 0, 0);
        let (_, _, _, _, _, fec) = nv(&pk[0]);
        let data_n = (fec >> 22) as usize;
        let mut blk = pk.clone();
        // the client clears the RTP/NV fields it rewrites before decoding? No:
        // parity was computed with them partly filled; data shards as sent
        // must reconstruct the payload bytes.
        assert!(rs_decode(&mut blk, data_n, &[1, 5]));
        assert_eq!(&blk[1][SHARD_HEADER..], &pk[1][SHARD_HEADER..]);
        assert_eq!(&blk[5][SHARD_HEADER..], &pk[5][SHARD_HEADER..]);
    }

    #[test]
    fn packet_index_outlives_the_16_bit_rtp_sequence() {
        let mut p = Packetizer::new(1024, 0, 0, None);
        p.seq = 65535;
        let pk = p.frame(&vec![1u8; 3000], 9, FrameType::P, 0, 0);
        assert_eq!(u16::from_be_bytes([pk[1][2], pk[1][3]]), 0); // RTP wraps
        assert_eq!(nv(&pk[1]).0 >> 8, 65536); // the 24-bit index does not
    }

    #[test]
    fn huge_frames_split_into_fec_blocks() {
        let mut p = Packetizer::new(1392, 20, 2, None);
        let data = vec![0x55u8; 600 * 1376];
        let pk = p.frame(&data, 3, FrameType::Idr, 0, 0);
        let blocks: std::collections::BTreeSet<u8> = pk.iter().map(|s| nv(s).4).collect();
        assert_eq!(blocks.len(), 3);
        assert!(blocks.iter().all(|b| b >> 6 == 2));
        // sequence numbers are contiguous across blocks
        for (i, s) in pk.iter().enumerate() {
            assert_eq!(u16::from_be_bytes([s[2], s[3]]), i as u16);
        }
    }

    #[test]
    fn encrypted_shards_carry_iv_frame_and_tag() {
        let key = [9u8; 16];
        let mut p = Packetizer::new(1024, 0, 0, Some(key));
        let pk = p.frame(&[1, 2, 3], 4, FrameType::P, 0, 0);
        assert_eq!(pk.len(), 1);
        let s = &pk[0];
        assert_eq!(s.len(), 32 + 1040);
        assert_eq!(s[11], b'V');
        assert_eq!(u32::from_le_bytes(s[12..16].try_into().unwrap()), 4);
        let pt = openssl::symm::decrypt_aead(
            Cipher::aes_128_gcm(),
            &key,
            Some(&s[..12]),
            &[],
            &s[32..],
            &s[16..32],
        )
        .unwrap();
        assert_eq!(&pt[32 + 8..32 + 11], &[1, 2, 3]);
    }
}
