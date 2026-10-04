//! Descriptor-chain layout for the vhost-user transport.
//!
//! Virtio lets a driver split a request and its reply across any number of
//! descriptors: device-readable ones first, then device-writable ones. The
//! Linux guest posts one of each, but another guest may post a header buffer
//! and a body buffer for the reply, so a reply is scattered across every
//! writable descriptor in order, never written into just one of them.

use virtio_queue::desc::split::Descriptor;
use vm_memory::{Bytes, GuestAddress};

/// One buffer of a chain: where it is in guest memory and how long.
pub type Segment = (GuestAddress, u32);

/// A chain whose descriptors are not readable-then-writable.
#[derive(Debug, PartialEq, Eq)]
pub struct ReadableAfterWritable;

/// Sort a chain's descriptors into its readable and writable buffers, in
/// chain order. Both lists are cleared first, so a caller can reuse them
/// from chain to chain without allocating.
///
/// A readable descriptor after a writable one is refused, as the spec
/// requires of a device: the request would otherwise be read out of order.
pub fn sort_chain(
    descs: impl IntoIterator<Item = Descriptor>,
    readable: &mut Vec<Segment>,
    writable: &mut Vec<Segment>,
) -> Result<(), ReadableAfterWritable> {
    readable.clear();
    writable.clear();
    for d in descs {
        if d.is_write_only() {
            writable.push((d.addr(), d.len()));
        } else if !writable.is_empty() {
            return Err(ReadableAfterWritable);
        } else {
            readable.push((d.addr(), d.len()));
        }
    }
    Ok(())
}

/// The writable buffers of a chain, in order, ignoring readable ones. For the
/// event queue, whose chains are all writable.
pub fn writable(descs: impl IntoIterator<Item = Descriptor>) -> Vec<Segment> {
    descs
        .into_iter()
        .filter(|d| d.is_write_only())
        .map(|d| (d.addr(), d.len()))
        .collect()
}

/// How many bytes the buffers hold together.
pub fn capacity(segs: &[Segment]) -> usize {
    segs.iter().map(|&(_, len)| len as usize).sum()
}

/// Write `bytes` across `segs` in order, filling each before the next.
/// Whatever does not fit in all of them together is cut off. Returns how many
/// bytes were written.
pub fn write_scattered<M: Bytes<GuestAddress>>(
    mem: &M,
    segs: &[Segment],
    bytes: &[u8],
) -> Result<usize, M::E> {
    let mut done = 0;
    for &(addr, len) in segs {
        if done == bytes.len() {
            break;
        }
        let n = (len as usize).min(bytes.len() - done);
        mem.write_slice(&bytes[done..done + n], addr)?;
        done += n;
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use virtio_bindings::bindings::virtio_ring::VRING_DESC_F_WRITE;
    use virtio_queue::desc::RawDescriptor;
    use virtio_queue::mock::MockSplitQueue;
    use virtio_queue::{Queue, QueueOwnedT};
    use vm_memory::GuestMemoryMmap;

    const BUF: u64 = 0x10_0000;

    fn mem() -> GuestMemoryMmap {
        GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x20_0000)]).unwrap()
    }

    fn read(mem: &GuestMemoryMmap, at: u64, len: usize) -> Vec<u8> {
        let mut v = vec![0u8; len];
        mem.read_slice(&mut v, GuestAddress(at)).unwrap();
        v
    }

    fn reply(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8 + 1).collect()
    }

    #[test]
    fn one_segment_takes_the_whole_reply() {
        let m = mem();
        let r = reply(100);
        let segs = [(GuestAddress(BUF), 4096)];
        assert_eq!(capacity(&segs), 4096);
        assert_eq!(write_scattered(&m, &segs, &r).unwrap(), 100);
        assert_eq!(read(&m, BUF, 100), r);
        // Nothing past the reply is touched.
        assert_eq!(read(&m, BUF + 100, 16), vec![0; 16]);
    }

    #[test]
    fn header_and_body_segments_split_the_reply() {
        let m = mem();
        let r = reply(200);
        // Not adjacent, so a write into the first alone would be caught.
        let segs = [(GuestAddress(BUF), 16), (GuestAddress(BUF + 0x1000), 4096)];
        assert_eq!(capacity(&segs), 16 + 4096);
        assert_eq!(write_scattered(&m, &segs, &r).unwrap(), 200);
        assert_eq!(read(&m, BUF, 16), r[..16]);
        assert_eq!(read(&m, BUF + 16, 16), vec![0; 16]);
        assert_eq!(read(&m, BUF + 0x1000, 184), r[16..]);
    }

    #[test]
    fn a_reply_longer_than_the_segments_is_cut_at_their_total() {
        let m = mem();
        let r = reply(100);
        let segs = [(GuestAddress(BUF), 16), (GuestAddress(BUF + 0x1000), 32)];
        assert_eq!(write_scattered(&m, &segs, &r).unwrap(), 48);
        assert_eq!(read(&m, BUF, 16), r[..16]);
        assert_eq!(read(&m, BUF + 0x1000, 32), r[16..48]);
        assert_eq!(read(&m, BUF + 0x1000 + 32, 16), vec![0; 16]);
    }

    #[test]
    fn no_segments_take_nothing() {
        let m = mem();
        assert_eq!(capacity(&[]), 0);
        assert_eq!(write_scattered(&m, &[], &reply(10)).unwrap(), 0);
    }

    #[test]
    fn an_empty_reply_writes_nothing() {
        let m = mem();
        let segs = [(GuestAddress(BUF), 16)];
        assert_eq!(write_scattered(&m, &segs, &[]).unwrap(), 0);
        assert_eq!(read(&m, BUF, 16), vec![0; 16]);
    }

    #[test]
    fn a_segment_outside_guest_memory_is_an_error() {
        let m = mem();
        let segs = [(GuestAddress(0x40_0000), 16)];
        assert!(write_scattered(&m, &segs, &reply(8)).is_err());
    }

    /// Post `descs` (addr, len, writable) as one chain on a mock queue and
    /// sort what the device side sees.
    fn sort_posted(
        m: &GuestMemoryMmap,
        descs: &[(u64, u32, bool)],
    ) -> Result<(Vec<Segment>, Vec<Segment>), ReadableAfterWritable> {
        let vq = MockSplitQueue::new(m, 16);
        let raw: Vec<RawDescriptor> = descs
            .iter()
            .map(|&(addr, len, w)| {
                let flags = if w { VRING_DESC_F_WRITE as u16 } else { 0 };
                RawDescriptor::from(Descriptor::new(addr, len, flags, 0))
            })
            .collect();
        vq.build_desc_chain(&raw).unwrap();
        let mut q: Queue = vq.create_queue().unwrap();
        let chain = q.iter(m).unwrap().next().unwrap();
        let (mut r, mut w) = (vec![(GuestAddress(1), 1)], vec![(GuestAddress(1), 1)]);
        sort_chain(chain, &mut r, &mut w)?;
        Ok((r, w))
    }

    #[test]
    fn a_posted_chain_sorts_into_readable_then_writable() {
        let m = mem();
        let (r, w) = sort_posted(
            &m,
            &[
                (BUF, 24, false),
                (BUF + 0x100, 40, false),
                (BUF + 0x1000, 16, true),
                (BUF + 0x2000, 4096, true),
            ],
        )
        .unwrap();
        assert_eq!(
            r,
            [(GuestAddress(BUF), 24), (GuestAddress(BUF + 0x100), 40)]
        );
        assert_eq!(
            w,
            [
                (GuestAddress(BUF + 0x1000), 16),
                (GuestAddress(BUF + 0x2000), 4096)
            ]
        );
    }

    #[test]
    fn the_linux_layout_is_one_of_each() {
        let m = mem();
        let (r, w) = sort_posted(&m, &[(BUF, 24, false), (BUF + 0x1000, 4096, true)]).unwrap();
        assert_eq!(r, [(GuestAddress(BUF), 24)]);
        assert_eq!(w, [(GuestAddress(BUF + 0x1000), 4096)]);
    }

    #[test]
    fn a_readable_after_a_writable_is_refused() {
        let m = mem();
        let got = sort_posted(
            &m,
            &[
                (BUF, 24, false),
                (BUF + 0x1000, 16, true),
                (BUF + 0x2000, 24, false),
            ],
        );
        assert_eq!(got, Err(ReadableAfterWritable));
    }

    #[test]
    fn writable_skips_readable_buffers() {
        let m = mem();
        let vq = MockSplitQueue::new(&m, 16);
        let raw = [
            RawDescriptor::from(Descriptor::new(BUF, 8, 0, 0)),
            RawDescriptor::from(Descriptor::new(
                BUF + 0x1000,
                16,
                VRING_DESC_F_WRITE as u16,
                0,
            )),
            RawDescriptor::from(Descriptor::new(
                BUF + 0x2000,
                64,
                VRING_DESC_F_WRITE as u16,
                0,
            )),
        ];
        vq.build_desc_chain(&raw).unwrap();
        let mut q: Queue = vq.create_queue().unwrap();
        let chain = q.iter(&m).unwrap().next().unwrap();
        assert_eq!(
            writable(chain),
            [
                (GuestAddress(BUF + 0x1000), 16),
                (GuestAddress(BUF + 0x2000), 64)
            ]
        );
    }
}
