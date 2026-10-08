//! The cursor queue (virtqueue 2): the hardware cursor's commands on their own path, never
//! behind the control queue's Venus traffic. The way virtio-gpu's own cursorq works, and the way a
//! bare-metal driver programs its cursor: a pointer update does not wait for rendering.
//!
//! # Availability
//!
//! Used only when all hold: the backend announces it (`NVGPU_CFG_CURSOR_QUEUE`, config `features`
//! bit 20, which it sets with the Venus cursor), the VMM exposes a third queue (QEMU's
//! `vhost-user-test-device-pci,num_vqs=3`: the device reports a nonzero size for queue 2), and the
//! knob `HwCursorQ` is not 0. Otherwise the cursor commands stay on the control queue, exactly as
//! before (`ctrl::set_cursor_blob`).
//!
//! # Shape
//!
//! One command at a time, in one preallocated buffer: `[MsgHeader{GpuCmd}, the command] ->
//! [MsgHeader, virtio_gpu_ctrl_hdr]`, the same message the control queue carries. No interrupt
//! (the avail ring asks for none, and no MSI-X vector is programmed for this queue): the sender
//! polls the used ring at PASSIVE for the answer, which a backend that serves this queue ahead of
//! the control queue gives within one control request. A command still unanswered when the
//! sender gives up stays in flight with its buffer; the next send reaps it first and, while it is
//! still out, is refused `QueueFull` (the caller keeps the command owed and sends it later).
//!
//! # Locking
//!
//! Every method runs under the adapter's virtio spinlock (`with_virtio`), at most DISPATCH: no
//! allocation, no wait. The buffer and the queue are made at `init` (PASSIVE) and live as long as
//! the transport, like the event queue's.

use super::*;

/// The queue's index.
pub(super) const CURSOR_QUEUE: u16 = 2;
/// Descriptors: one chain of four (two reads, two writes), with room to spare.
const CURSOR_QUEUE_SIZE: usize = 8;
/// The command bytes the buffer holds (a `HeliosSetCursorBlob` is far smaller).
const CMD_MAX: usize = 256;
/// The response after the header: one `virtio_gpu_ctrl_hdr`.
const RESP_BYTES: usize = core::mem::size_of::<helios_protocol::VirtioGpuCtrlHdr>();

const _: () = assert!(core::mem::size_of::<helios_protocol::HeliosSetCursorBlob>() <= CMD_MAX);

/// The cursor queue, its one buffer, and the command in flight.
pub(super) struct CursorRing {
    queue: VirtQueue<WdkHal, CURSOR_QUEUE_SIZE>,
    buf: DmaBuffer,
    /// The token of the command on the device, with its length.
    inflight: Option<(u16, usize)>,
}

/// Build the cursor queue (before `DRIVER_OK`). `None` when the device has no queue 2 or the
/// memory is refused: the cursor then stays on the control queue. PASSIVE.
///
/// The buffer is allocated FIRST: `VirtQueue::new` enables the queue on the device as its last
/// act, and a queue that exists must never be dropped before the device is reset.
pub(super) fn new_cursor_ring(
    passive: crate::irql::PassiveLevel,
    transport: &mut PciTransport,
) -> Option<Box<CursorRing>> {
    if transport.max_queue_size(CURSOR_QUEUE) < CURSOR_QUEUE_SIZE as u32 {
        return None;
    }
    let buf = DmaBuffer::new(passive, CMD_MAX + RESP_BYTES)?;
    let mut queue =
        VirtQueue::<WdkHal, CURSOR_QUEUE_SIZE>::new(transport, CURSOR_QUEUE, false, false).ok()?;
    // No interrupt: the sender polls (see the module docs).
    queue.set_dev_notify(false);
    Some(Box::new(CursorRing {
        queue,
        buf,
        inflight: None,
    }))
}

/// What a poll found.
pub enum CursorPoll {
    /// Nothing in flight.
    Idle,
    /// Still on the device.
    Pending,
    /// Answered: `true` for a success answer.
    Done(bool),
}

impl CursorRing {
    /// The chain's spans over the one buffer, for a command of `len` bytes.
    fn spans(&self, len: usize) -> Option<(DmaSpan, DmaSpan, DmaSpan, DmaSpan)> {
        let (req_hdr, resp_hdr) = self.buf.wire_spans();
        Some((
            req_hdr,
            self.buf.span(0, len)?,
            resp_hdr,
            self.buf.span(CMD_MAX, RESP_BYTES)?,
        ))
    }

    /// Take the answer of the command in flight, if it came.
    fn poll(&mut self) -> CursorPoll {
        let Some((token, len)) = self.inflight else {
            return CursorPoll::Idle;
        };
        if self.queue.peek_used() != Some(token) {
            return CursorPoll::Pending;
        }
        let Some((rh, cmd, sh, resp)) = self.spans(len) else {
            return CursorPoll::Pending;
        };
        // SAFETY: the same spans `send` added under this token; the device is done with them
        // once `pop_used` succeeds.
        let popped = unsafe {
            self.queue.pop_used(
                token,
                &[rh.as_slice(), cmd.as_slice()],
                &mut [sh.as_mut_slice(), resp.as_mut_slice()],
            )
        };
        self.inflight = None;
        if popped.is_err() {
            return CursorPoll::Done(false);
        }
        // SAFETY: the device returned the buffer; the response span is ours to read.
        let r = unsafe { resp.as_slice() };
        let resp_type = u32::from_le_bytes([r[0], r[1], r[2], r[3]]);
        CursorPoll::Done(self.buf.wire_status() >= 0 && helios_protocol::resp_is_ok(resp_type))
    }
}

impl VirtioGpu {
    /// Whether cursor commands go on the cursor queue (fixed for the transport).
    pub fn cursor_queue_on(&self) -> bool {
        self.cursor_ring.is_some()
    }

    /// Put one cursor command on the cursor queue and ring its doorbell. `QueueFull` while the
    /// previous one is still out (it is reaped first if it came back meanwhile).
    pub fn cursor_send(&mut self, cmd: &[u8]) -> Result<(), VirtioError> {
        if self.failed {
            return Err(VirtioError::DeviceError);
        }
        let Some(ring) = self.cursor_ring.as_mut() else {
            return Err(VirtioError::DeviceError);
        };
        if matches!(ring.poll(), CursorPoll::Pending) {
            return Err(VirtioError::QueueFull);
        }
        if cmd.is_empty() || cmd.len() > CMD_MAX {
            return Err(VirtioError::DeviceError);
        }
        ring.buf.as_mut_slice()[..cmd.len()].copy_from_slice(cmd);
        let Some((rh, c, sh, resp)) = ring.spans(cmd.len()) else {
            return Err(VirtioError::DeviceError);
        };
        // SAFETY: spans of the ring's own buffer, which lives as long as the queue and is not
        // touched again until `poll` pops this chain.
        let added = unsafe {
            ring.queue.add(
                &[rh.as_slice(), c.as_slice()],
                &mut [sh.as_mut_slice(), resp.as_mut_slice()],
            )
        };
        let token = match added {
            Ok(token) => token,
            Err(virtio_drivers::Error::QueueFull) => return Err(VirtioError::QueueFull),
            Err(_) => return Err(VirtioError::DeviceError),
        };
        ring.inflight = Some((token, cmd.len()));
        if ring.queue.should_notify() {
            self.transport.notify(CURSOR_QUEUE);
        }
        Ok(())
    }

    /// The answer of the command in flight, if it came.
    pub fn cursor_poll(&mut self) -> CursorPoll {
        match self.cursor_ring.as_mut() {
            Some(ring) => ring.poll(),
            None => CursorPoll::Idle,
        }
    }
}
