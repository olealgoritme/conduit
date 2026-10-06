//! Host `EventReady` -> the usermode events a process registered for its RM
//! handles (`HELIOS_NVRM_OP_EVENT_REGISTER`), and the loss of the transport ->
//! every registered event.
//!
//! # Where the notifications come from
//!
//! The device's second virtqueue (index 1, the "event" queue) carries buffers
//! the DRIVER posts empty and the HOST fills: `EventReady` (a bare 16-byte
//! `MsgHeader { msg_type = 8, handle, status, pad }`, "the backend file `handle`
//! became readable"), and, for a guest that takes input, `InputEvent` /
//! `DisplayMode` / clipboard messages. This KMD wants only `EventReady`.
//!
//! # Availability, and the input bit
//!
//! No feature bit is needed for `EventReady`: the host offers every handle the
//! backend returns from `Open` to its event pump, which sends `EventReady` on this
//! queue once the guest has it up with buffers posted. So events are available
//! exactly when this KMD managed to bring the queue up ([`new_event_ring`]); if
//! not (a device without a second queue, no contiguous memory) `EVENT_REGISTER`
//! answers `UNSUPPORTED`.
//!
//! Virtio feature bit 12 (`NVGPU_CFG_TAKES_INPUT`) is a different matter: a driver
//! that ACKS it gets the keyboard and mouse as `InputEvent`s on this queue instead
//! of the emulated devices, and the Windows driver cannot consume them. This KMD
//! therefore never acks it and does not read it: `init` acks only
//! `CONDUIT_REQUIRED_FEATURES`, and an assertion below keeps bit 12 out of it. A
//! host only sends `InputEvent` to a guest that acked the bit; anything but
//! `EventReady` that does arrive is counted (`NvEvOther`) and dropped.
//!
//! # Locking and IRQL
//!
//! Every method here runs under the adapter's virtio spinlock (DISPATCH_LEVEL):
//! no allocation (the registry and the buffer ring are sized at init), no wait,
//! `KeSetEvent(Wait = FALSE)` only. Dropping an event's object reference is
//! PASSIVE-only, so no method here drops one: those that remove a registration
//! hand the event back by value and the caller releases it with
//! [`release_nvrm_event`] after the lock is gone.

use super::*;
use crate::virtio::nvrm::{
    NVRM_EV_DROPS, NVRM_EV_ERRORS, NVRM_EV_LATCHED, NVRM_EV_LOST, NVRM_EV_OTHER, NVRM_EV_SIGNALS,
};
use helios_kmd_logic::nvrm_events::{kind_known, Added, KINDS_ALL, KIND_LOST, KIND_READY};

/// Most registrations across every process (each is one object reference).
pub const MAX_NVRM_EVENTS: usize = 1024;
/// Most one process may hold: a `READY` registration per handle it can have open,
/// plus its `TRANSPORT_LOST` one.
pub const MAX_NVRM_EVENTS_PER_OWNER: usize = MAX_NVRM_HANDLES_PER_OWNER + 1;

/// Virtio feature bit 12 (`NVGPU_CFG_TAKES_INPUT`), which this driver must never
/// ack (see the module docs). Named only for the assertion below.
const NEVER_ACKED_TAKES_INPUT: u64 = 1 << 12;

/// The event queue's index and size (16 buffers; see `EVENT_QUEUE_SIZE`).
pub(super) const EVENT_QUEUE: u16 = 1;
/// Small on purpose: `VirtQueue<_, N>::new` returns a by-value slot that grows
/// with N and sits on the boot stack under `VirtioGpu::init` (see
/// tools/kmd-frame-sizes.ps1). Events are rare and level-triggered on the host,
/// so a handful of buffers is plenty.
const EVENT_QUEUE_SIZE: usize = 16;
/// Bytes of one posted buffer: room for the 16-byte `EventReady`, and, should a
/// host send one anyway, a short `DisplayMode` (anything longer is the host's to
/// truncate or drop; this driver only reads the header). Divides a page, so no
/// buffer straddles one.
const EVENT_BUF_BYTES: usize = 256;

/// Messages other than `EventReady` after which the queue is no longer kicked on
/// repost (see `drain_nvrm_events`).
const OTHER_KICK_LIMIT: u32 = 1024;
/// Host `MsgType::EventReady`.
const MSG_EVENT_READY: u32 = 8;

// Bit 12 must never become part of what `init` acks.
const _: () = {
    assert!(CONDUIT_REQUIRED_FEATURES & NEVER_ACKED_TAKES_INPUT == 0);
    // The registry's kinds are the ABI's.
    assert!(KIND_READY == helios_protocol::HELIOS_NVRM_EVENT_READY);
    assert!(KIND_LOST == helios_protocol::HELIOS_NVRM_EVENT_TRANSPORT_LOST);
    assert!(KINDS_ALL == helios_protocol::HELIOS_NVRM_EVENT_KINDS_ALL);
    // Buffers tile the DMA area exactly, one page holds a whole number of them,
    // and a slot index fits the `u8` token map.
    assert!(4096 % EVENT_BUF_BYTES == 0 && EVENT_BUF_BYTES >= 16);
    assert!(EVENT_QUEUE_SIZE <= 256 && EVENT_QUEUE_SIZE.is_power_of_two());
};

/// The event virtqueue and the buffers posted on it.
pub(super) struct EventRing {
    queue: VirtQueue<WdkHal, EVENT_QUEUE_SIZE>,
    /// `EVENT_QUEUE_SIZE` buffers of `EVENT_BUF_BYTES`, in contiguous memory the
    /// device writes. Owned here for as long as the queue exists; freed (PASSIVE)
    /// only after the transport's reset.
    bufs: DmaBuffer,
    /// Descriptor token -> index of the buffer posted under it.
    slot_of_token: [u8; EVENT_QUEUE_SIZE],
}

/// Build the event queue (before `DRIVER_OK`). `None` if the memory or the queue
/// is refused: events are then simply unavailable. PASSIVE.
///
/// The buffers are allocated FIRST. `VirtQueue::new` enables the queue on the
/// device as its last act, and a queue that exists must never be dropped before
/// the device is reset; nothing fallible follows it.
#[inline(never)]
pub(super) fn new_event_ring(
    passive: crate::irql::PassiveLevel,
    transport: &mut PciTransport,
) -> Option<Box<EventRing>> {
    let bufs = DmaBuffer::new(passive, EVENT_QUEUE_SIZE * EVENT_BUF_BYTES)?;
    let mut queue =
        VirtQueue::<WdkHal, EVENT_QUEUE_SIZE>::new(transport, EVENT_QUEUE, false, false).ok()?;
    // An interrupt per filled buffer: the DPC drains the queue.
    queue.set_dev_notify(true);
    Some(Box::new(EventRing {
        queue,
        bufs,
        slot_of_token: [0; EVENT_QUEUE_SIZE],
    }))
}

/// Put buffer `slot` on the queue for the host to fill. `false` if the queue
/// would not take it (the buffer is then lost to this ring for good).
fn post_slot(ring: &mut EventRing, slot: usize) -> bool {
    let Some(span) = ring.bufs.span(slot * EVENT_BUF_BYTES, EVENT_BUF_BYTES) else {
        return false;
    };
    // SAFETY: a sub-span of the ring's own contiguous `DmaBuffer`, which lives as
    // long as the queue and is not touched by the driver again until the device
    // returns it through `pop_used` (`take_event`).
    let added = unsafe { ring.queue.add(&[], &mut [span.as_mut_slice()]) };
    match added {
        Ok(token) => {
            if let Some(s) = ring.slot_of_token.get_mut(usize::from(token)) {
                *s = slot as u8;
            }
            true
        }
        Err(_) => false,
    }
}

/// What came back from the event queue.
enum Taken {
    /// The queue is empty.
    Empty,
    /// `EventReady` for this backend handle.
    Ready(u32),
    /// Some other message (or a short or bad one): dropped.
    Other,
    /// The queue misbehaved: stop draining this pass.
    Broken,
}

/// Take one filled buffer off the queue, read it, and give the buffer straight
/// back to the host.
fn take_event(ring: &mut EventRing) -> Taken {
    let Some(token) = ring.queue.peek_used() else {
        return Taken::Empty;
    };
    let Some(slot) = ring
        .slot_of_token
        .get(usize::from(token))
        .map(|s| usize::from(*s))
    else {
        return Taken::Broken;
    };
    let Some(span) = ring.bufs.span(slot * EVENT_BUF_BYTES, EVENT_BUF_BYTES) else {
        return Taken::Broken;
    };
    // SAFETY: the same sub-span `post_slot` added under this token.
    let used = unsafe { ring.queue.pop_used(token, &[], &mut [span.as_mut_slice()]) };
    let Ok(len) = used else {
        return Taken::Broken;
    };
    // The host's bytes, now ours until the buffer is posted again below.
    // SAFETY: the device is done with the buffer (`pop_used` succeeded).
    let bytes = unsafe { span.as_slice() };
    let taken = {
        let len = (len as usize).min(bytes.len());
        let word = |at: usize| {
            bytes
                .get(at..at + 4)
                .and_then(|b| <[u8; 4]>::try_from(b).ok())
                .map(u32::from_le_bytes)
        };
        match (len >= 16, word(0), word(4)) {
            (true, Some(MSG_EVENT_READY), Some(handle)) => Taken::Ready(handle),
            _ => Taken::Other,
        }
    };
    if !post_slot(ring, slot) {
        NVRM_EV_ERRORS.fetch_add(1, Ordering::Relaxed);
    }
    taken
}

/// Drop one reference on a registered event. PASSIVE, outside every lock.
pub fn release_nvrm_event(event: NonNull<KEVENT>) {
    // SAFETY: the registration owned one reference, handed over by the table
    // method that removed it; released exactly once, here.
    unsafe { wdk_sys::ntddk::ObfDereferenceObject(event.as_ptr() as PVOID) };
}

/// Whether registering events can work right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NvrmEventsState {
    /// The event queue could not be brought up (the device has no second queue, or
    /// no memory for its buffers).
    Unavailable,
    /// The transport has failed: nothing registered will ever fire again.
    Lost,
    /// Usable.
    Ready,
}

/// Why `register_nvrm_event` refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NvrmEventRefusal {
    Unavailable,
    TransportLost,
    /// `READY` for a handle the caller did not open.
    NotOwned,
    /// The per-process or the device-wide table is full.
    NoResources,
}

/// A registration that took.
pub struct NvrmEventRegistered {
    /// The event this registration replaced; the caller releases it.
    pub replaced: Option<NonNull<KEVENT>>,
    /// A notification was latched for the handle: the event was signalled.
    pub latched: bool,
}

impl VirtioGpu {
    /// Whether events are usable; see [`NvrmEventsState`].
    pub fn nvrm_events_state(&self) -> NvrmEventsState {
        if self.nvrm_event_ring.is_none() {
            NvrmEventsState::Unavailable
        } else if self.failed {
            NvrmEventsState::Lost
        } else {
            NvrmEventsState::Ready
        }
    }

    /// The bitmask `QUERY_CAPS` reports as `supported_event_kinds`.
    pub fn nvrm_event_kinds(&self) -> u32 {
        if self.nvrm_events_state() == NvrmEventsState::Ready {
            KINDS_ALL
        } else {
            0
        }
    }

    /// The device's config `features` word (`NVGPU_CFG_*`), read at init.
    pub fn nvrm_device_features(&self) -> u32 {
        self.cfg_features
    }

    /// Give the host its buffers. Once, from `StartDevice` after the ISR address is
    /// published (a host push landing in a posted buffer raises the interrupt).
    /// PASSIVE (or any IRQL: nothing here allocates).
    pub fn post_nvrm_event_buffers(&mut self) {
        let Some(ring) = self.nvrm_event_ring.as_mut() else {
            return;
        };
        let mut posted = 0usize;
        for slot in 0..EVENT_QUEUE_SIZE {
            if !post_slot(ring, slot) {
                NVRM_EV_ERRORS.fetch_add(1, Ordering::Relaxed);
                break;
            }
            posted += 1;
        }
        let notify = posted != 0 && ring.queue.should_notify();
        if notify {
            self.transport.notify(EVENT_QUEUE);
        }
    }

    /// Register `event` (one object reference, which the table takes over on
    /// success) for `(owner, handle, kind)`. A `TRANSPORT_LOST` registration has
    /// no handle. On `Err` the caller still owns its reference.
    pub fn register_nvrm_event(
        &mut self,
        owner: DeviceOwner,
        handle: u32,
        kind: u32,
        event: NonNull<KEVENT>,
    ) -> Result<NvrmEventRegistered, NvrmEventRefusal> {
        if !kind_known(kind) {
            return Err(NvrmEventRefusal::Unavailable);
        }
        match self.nvrm_events_state() {
            NvrmEventsState::Unavailable => return Err(NvrmEventRefusal::Unavailable),
            NvrmEventsState::Lost => return Err(NvrmEventRefusal::TransportLost),
            NvrmEventsState::Ready => {}
        }
        let key_handle = if kind == KIND_LOST { 0 } else { handle };
        // Checked and recorded under this one lock hold, so a registration can
        // never name a handle a concurrent `Close` has already taken.
        if kind == KIND_READY && !self.nvrm_handle_owned(owner, key_handle) {
            return Err(NvrmEventRefusal::NotOwned);
        }
        let replaced = match self.nvrm_events.add(owner.raw(), key_handle, kind, event) {
            Added::New => None,
            Added::Replaced(old) => Some(old),
            Added::TotalFull | Added::OwnerFull => return Err(NvrmEventRefusal::NoResources),
        };
        // A notification that arrived before there was anything to wake.
        let latched = kind == KIND_READY && self.take_nvrm_ready_latch(owner, key_handle);
        if latched {
            // SAFETY: the table holds a reference to the event; Wait = FALSE is
            // legal at DISPATCH_LEVEL under a spinlock.
            unsafe { KeSetEvent(event.as_ptr(), IO_NO_INCREMENT, 0) };
            NVRM_EV_SIGNALS.fetch_add(1, Ordering::Relaxed);
        }
        Ok(NvrmEventRegistered { replaced, latched })
    }

    /// Remove `(owner, handle, kind)`; the caller releases the event it returns.
    pub fn unregister_nvrm_event(
        &mut self,
        owner: DeviceOwner,
        handle: u32,
        kind: u32,
    ) -> Option<NonNull<KEVENT>> {
        let key_handle = if kind == KIND_LOST { 0 } else { handle };
        self.nvrm_events.remove(owner.raw(), key_handle, kind)
    }

    /// `Close` of `handle`: pop one registration `owner` has on it.
    pub fn take_nvrm_event_for_handle(
        &mut self,
        owner: DeviceOwner,
        handle: u32,
    ) -> Option<NonNull<KEVENT>> {
        self.nvrm_events.take_for_handle(owner.raw(), handle)
    }

    /// Device teardown: pop one registration `owner` still holds.
    pub fn take_nvrm_event_for_owner(&mut self, owner: DeviceOwner) -> Option<NonNull<KEVENT>> {
        self.nvrm_events.take_for_owner(owner.raw())
    }

    /// Wake every registration, of every kind: the transport is gone. Does not
    /// remove them (the owners' `Close` / exit, or the transport's `Drop`, do).
    pub(super) fn signal_nvrm_events_lost(&mut self) {
        let n = self.nvrm_events.signal_all(|event| {
            // SAFETY: the table holds a reference to each event; Wait = FALSE is
            // legal at DISPATCH_LEVEL under a spinlock.
            unsafe { KeSetEvent(event.as_ptr(), IO_NO_INCREMENT, 0) };
        });
        NVRM_EV_LOST.fetch_add(n as u32, Ordering::Relaxed);
    }

    /// Release what the transport's `Drop` finds registered: wake it (the owners
    /// see the loss) and drop the references. PASSIVE, outside the device lock.
    pub(super) fn teardown_nvrm_events(&mut self) {
        self.signal_nvrm_events_lost();
        while let Some(event) = self.nvrm_events.take_any() {
            release_nvrm_event(event);
        }
    }

    /// One `EventReady{handle}`: wake the registered events; failing that, latch
    /// it on the handle so the next `REGISTER` wakes at once. A handle nobody has
    /// open (a host fence's, a closed file's) is dropped.
    fn deliver_nvrm_ready(&mut self, handle: u32) {
        let woke = self.nvrm_events.signal_handle(handle, KIND_READY, |event| {
            // SAFETY: the table holds a reference to the event; Wait = FALSE is
            // legal at DISPATCH_LEVEL under a spinlock.
            unsafe { KeSetEvent(event.as_ptr(), IO_NO_INCREMENT, 0) };
        });
        if woke != 0 {
            NVRM_EV_SIGNALS.fetch_add(woke as u32, Ordering::Relaxed);
        } else if self.latch_nvrm_ready(handle) {
            NVRM_EV_LATCHED.fetch_add(1, Ordering::Relaxed);
        } else {
            NVRM_EV_DROPS.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// The event queue's consumer, from the interrupt DPC (under the virtio
    /// lock): hand each `EventReady` to the registered events and give the
    /// buffers back. At most one ring's worth per call, so a host that floods it
    /// cannot hold the DPC; the rest waits for the next interrupt.
    pub fn drain_nvrm_events(&mut self) {
        if self.failed {
            return;
        }
        let mut reposted = false;
        for _ in 0..EVENT_QUEUE_SIZE {
            let Some(ring) = self.nvrm_event_ring.as_mut() else {
                return;
            };
            match take_event(ring) {
                Taken::Empty => break,
                Taken::Ready(handle) => {
                    reposted = true;
                    self.deliver_nvrm_ready(handle);
                }
                Taken::Other => {
                    reposted = true;
                    NVRM_EV_OTHER.fetch_add(1, Ordering::Relaxed);
                }
                Taken::Broken => {
                    NVRM_EV_ERRORS.fetch_add(1, Ordering::Relaxed);
                    break;
                }
            }
        }
        // A host that serves queue-1 kicks as requests would answer every kick
        // with another message and keep this loop spinning at DPC rate (an old
        // backend did): past this many dropped messages, stop kicking. Current
        // backends need no kick for it; the buffers stay posted.
        if reposted && NVRM_EV_OTHER.load(Ordering::Relaxed) <= OTHER_KICK_LIMIT {
            let notify = self
                .nvrm_event_ring
                .as_ref()
                .is_some_and(|r| r.queue.should_notify());
            if notify {
                self.transport.notify(EVENT_QUEUE);
            }
        }
    }
}
