//! Audio: designed, not yet fed (the VM has no sound device yet).
//!
//! The path once virtio-sound exists (see docs/STREAMING.md):
//!   PipeWire/Pulse monitor of the VM's sink "conduit-NAME" → 48 kHz PCM →
//!   Opus, `packet_ms` per packet → RTP (type 97, seq, timestamp += samples) →
//!   every 4 data packets 2 Reed-Solomon parity packets with GameStream's fixed
//!   parity matrix → AES-128-CBC with the launch key (IV = rikeyid BE + seq) when
//!   the client enabled audio encryption → UDP to the session's audio peer.
//!
//! What exists: the port answers pings (udp.rs records the peer), RTSP
//! advertises the Opus layouts, and the packetizer below is the RTP + FEC
//! framing, tested, ready for a source.

use super::video::rs_encode;

pub const DATA_SHARDS: usize = 4;
pub const FEC_SHARDS: usize = 2;
/// GameStream's audio parity matrix (OpenFEC's 4+2), which a generic
/// Reed-Solomon construction does not reproduce.
pub const PARITY: [u8; 8] = [0x77, 0x40, 0x38, 0x0e, 0xc7, 0xa7, 0x0d, 0x6c];

/// RTP framing + FEC for audio packets of a fixed size per stream.
pub struct AudioPacketizer {
    seq: u16,
    ts: u32,
    samples_per_packet: u32,
    shards: Vec<Vec<u8>>,
    base_seq: u16,
    base_ts: u32,
}

impl AudioPacketizer {
    pub fn new(packet_ms: u32) -> Self {
        AudioPacketizer {
            seq: 0,
            ts: 0,
            samples_per_packet: packet_ms * 48,
            shards: Vec::new(),
            base_seq: 0,
            base_ts: 0,
        }
    }

    /// One (already encrypted, if enabled) Opus packet → datagrams to send:
    /// the data packet, plus the 2 parity packets after every 4th.
    pub fn packet(&mut self, payload: &[u8]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut d = vec![0x80u8, 97];
        d.extend_from_slice(&self.seq.to_be_bytes());
        d.extend_from_slice(&self.ts.to_be_bytes());
        d.extend_from_slice(&0u32.to_be_bytes());
        d.extend_from_slice(payload);
        out.push(d);
        if self.shards.is_empty() {
            self.base_seq = self.seq;
            self.base_ts = self.ts;
        }
        self.shards.push(payload.to_vec());
        self.seq = self.seq.wrapping_add(1);
        self.ts = self.ts.wrapping_add(self.samples_per_packet);
        if self.shards.len() == DATA_SHARDS {
            let bs = self.shards.iter().map(Vec::len).max().unwrap_or(0);
            let mut all: Vec<Vec<u8>> = self
                .shards
                .drain(..)
                .map(|mut s| {
                    s.resize(bs, 0);
                    s
                })
                .collect();
            all.extend((0..FEC_SHARDS).map(|_| vec![0u8; bs]));
            rs_encode(&mut all, DATA_SHARDS, Some(&PARITY));
            for (i, p) in all[DATA_SHARDS..].iter().enumerate() {
                // RTP (type 127) + AUDIO_FEC_HEADER {index, payloadType 97, baseSeq, baseTs, ssrc}
                let mut f = vec![0x80u8, 127];
                f.extend_from_slice(
                    &(self.base_seq.wrapping_add((DATA_SHARDS + i) as u16)).to_be_bytes(),
                );
                f.extend_from_slice(&0u32.to_be_bytes());
                f.extend_from_slice(&0u32.to_be_bytes());
                f.push(i as u8);
                f.push(97);
                f.extend_from_slice(&self.base_seq.to_be_bytes());
                f.extend_from_slice(&self.base_ts.to_be_bytes());
                f.extend_from_slice(&0u32.to_be_bytes());
                f.extend_from_slice(p);
                out.push(f);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_fourth_packet_brings_two_parity_packets() {
        let mut a = AudioPacketizer::new(5);
        let mut n = 0;
        for i in 0..8u8 {
            let out = a.packet(&[i; 40]);
            n += out.len();
            assert_eq!(out[0][1], 97);
            assert_eq!(u16::from_be_bytes([out[0][2], out[0][3]]), i as u16);
            assert_eq!(
                u32::from_be_bytes(out[0][4..8].try_into().unwrap()),
                i as u32 * 240
            );
            if i % 4 == 3 {
                assert_eq!(out.len(), 3);
                assert_eq!(out[1][1], 127);
                assert_eq!(out[1].len(), 12 + 12 + 40);
            }
        }
        assert_eq!(n, 12);
    }
}
