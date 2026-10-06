//! Message-signalled interrupt (MSI / MSI-X) decisions for the virtio-gpu
//! device: which vector each source gets, how many messages the OS granted,
//! and what the ISR does with a message number.
//!
//! Pure functions of their arguments: no wdk, no atomics, no MMIO. The driver
//! feeds them values it read from PCI config space and the translated resource
//! list, and acts on what they return. See `docs/msi-interrupts.md`.
//!
//! The one rule everything here serves: **never name a vector the OS did not
//! grant.** A vector the device signals but the OS never connected is a lost
//! interrupt, i.e. a hang. Every function therefore errs towards FEWER vectors.

/// `VIRTIO_MSI_NO_VECTOR`: "do not signal this source with a message". Also what
/// a device reads back from `msix_config` / `queue_msix_vector` when it refused
/// the vector that was written.
pub const NO_VECTOR: u16 = 0xFFFF;

/// Queues one plan can describe. The control and event queues are the two in use;
/// the room is for the RM queue that is planned.
pub const MAX_QUEUES: usize = 4;

/// Vector assignment for the device's interrupt sources.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Plan {
    /// Vector for configuration-change interrupts, or [`NO_VECTOR`].
    pub config: u16,
    /// Vector per queue index, [`NO_VECTOR`] for an index the plan does not cover.
    pub queue: [u16; MAX_QUEUES],
}

impl Plan {
    /// Every source unassigned.
    pub const NONE: Plan = Plan {
        config: NO_VECTOR,
        queue: [NO_VECTOR; MAX_QUEUES],
    };

    /// Everything on message 0 (queues; config stays unassigned, see [`plan`]).
    pub const fn all_shared(queues: usize) -> Plan {
        let mut queue = [NO_VECTOR; MAX_QUEUES];
        let mut i = 0;
        while i < queues && i < MAX_QUEUES {
            queue[i] = 0;
            i += 1;
        }
        Plan {
            config: NO_VECTOR,
            queue,
        }
    }

    /// Whether any source uses a message. A plan that does not is not MSI.
    pub fn uses_messages(&self) -> bool {
        self.config != NO_VECTOR || self.queue.iter().any(|&v| v != NO_VECTOR)
    }

    /// The highest message number the plan uses, if any.
    pub fn max_vector(&self) -> Option<u16> {
        self.queue
            .iter()
            .copied()
            .chain(core::iter::once(self.config))
            .filter(|&v| v != NO_VECTOR)
            .max()
    }
}

/// Assign vectors given `granted` messages (a LOWER BOUND on what the OS
/// connected; see [`granted_messages`]) and `queues` queues.
///
/// * `granted == 0`: nothing. The caller is not in MSI mode.
/// * `granted == 1`, or `shared_only`: one shared message 0 for every queue. The
///   config vector is left unassigned because the ISR could not tell a config
///   change from queue work on a shared message, and the ISR-status register that
///   would say is not read in message mode. The Conduit device raises no
///   config-change interrupt.
/// * otherwise: message 0 is config only and queue `i` gets message `i + 1`,
///   clamped to the last granted message (so with 2 granted, every queue shares
///   message 1 and config is still distinguishable).
///
/// Table-driven on purpose: a third queue is a larger `queues`, nothing else.
pub const fn plan(granted: u32, queues: usize, shared_only: bool) -> Plan {
    if granted == 0 {
        return Plan::NONE;
    }
    if granted == 1 || shared_only {
        return Plan::all_shared(queues);
    }
    let last = granted - 1; // >= 1
    let mut queue = [NO_VECTOR; MAX_QUEUES];
    let mut i = 0;
    while i < queues && i < MAX_QUEUES {
        let want = i as u32 + 1;
        queue[i] = (if want > last { last } else { want }) as u16;
        i += 1;
    }
    Plan { config: 0, queue }
}

/// A device that took a vector reads it back unchanged; [`NO_VECTOR`] (or any
/// other value) means it refused.
pub const fn vector_accepted(wrote: u16, read_back: u16) -> bool {
    wrote == read_back
}

// ── PCI capability bits ──────────────────────────────────────────────────────

/// PCI capability ids.
pub const PCI_CAP_ID_MSIX: u8 = 0x11;

/// MSI-X Message Control (config dword at `cap`, bits 31:16): bit 15 = enable.
pub const fn msix_enabled(message_control: u16) -> bool {
    message_control & (1 << 15) != 0
}

/// MSI-X table size (`N`, encoded as `N - 1` in bits 10:0).
pub const fn msix_table_size(message_control: u16) -> u32 {
    (message_control & 0x7FF) as u32 + 1
}

// ── Translated resource list ─────────────────────────────────────────────────
//
// `CM_RESOURCE_LIST` as dxgkrnl hands it in `DXGK_DEVICE_INFO.TranslatedResourceList`
// (x64, every struct `pshpack4`):
//
//   0   ULONG Count                      full descriptors
//   4   CM_FULL_RESOURCE_DESCRIPTOR[0]
//   4     INTERFACE_TYPE InterfaceType
//   8     ULONG BusNumber
//   12    USHORT Version, USHORT Revision
//   16    ULONG Count                    partial descriptors
//   20    CM_PARTIAL_RESOURCE_DESCRIPTOR[Count], 20 bytes each:
//           +0 UCHAR Type, UCHAR ShareDisposition, USHORT Flags, +4 union u[16]

const CM_RESOURCE_TYPE_INTERRUPT: u32 = 2;
const CM_RESOURCE_TYPE_DEVICE_SPECIFIC: u32 = 5;
/// `CM_RESOURCE_INTERRUPT_MESSAGE`.
const CM_RESOURCE_INTERRUPT_MESSAGE: u32 = 0x0002;
const PARTIAL_LIST_COUNT_OFFSET: usize = 16;
const FIRST_PARTIAL_OFFSET: usize = 20;
const PARTIAL_SIZE: usize = 20;
/// A display adapter has a handful of resources. Anything beyond this is a list
/// this parser does not understand, and it stops there (a lower bound).
const MAX_PARTIALS: u32 = 64;

/// How many message-signalled interrupt descriptors the first full resource
/// descriptor carries: a LOWER BOUND on the messages the OS connected. `read`
/// returns the little-endian dword at a 4-aligned byte offset into the list, or
/// `None` when the offset is out of range; the parser never asks for more than
/// the list's own counts describe.
///
/// Whether the OS expresses N messages as one descriptor with a count or as N
/// descriptors is not something a driver may assume, so this counts descriptors
/// only: it can under-count (then the plan shares vectors), never over-count.
pub fn granted_messages(read: impl Fn(usize) -> Option<u32>) -> u32 {
    let Some(full) = read(0) else { return 0 };
    if full == 0 {
        return 0;
    }
    let Some(partials) = read(PARTIAL_LIST_COUNT_OFFSET) else {
        return 0;
    };
    let partials = partials.min(MAX_PARTIALS) as usize;
    let mut messages = 0u32;
    for i in 0..partials {
        let Some(head) = read(FIRST_PARTIAL_OFFSET + i * PARTIAL_SIZE) else {
            break;
        };
        let ty = head & 0xFF;
        if ty == CM_RESOURCE_TYPE_DEVICE_SPECIFIC {
            // Variable length: everything after it is at an offset this parser
            // cannot know. Stop with what was counted.
            break;
        }
        let flags = head >> 16;
        if ty == CM_RESOURCE_TYPE_INTERRUPT && flags & CM_RESOURCE_INTERRUPT_MESSAGE != 0 {
            messages += 1;
        }
    }
    messages
}

/// Messages to plan for, given the device's MSI-X Message Control (`None` when it
/// has no MSI-X capability) and the number of message descriptors in the
/// translated resource list (see [`granted_messages`]).
///
/// Two independent signals that the OS connected messages rather than the INTx
/// line: the MSI-X Enable bit, and message descriptors in the resource list.
/// EITHER is enough, because each can lag the other (the OS may set Enable only
/// when it connects, and a resource-list layout this parser mis-reads counts as
/// zero); a driver that acts only on the signal that happened to be late would
/// leave an enabled device with no vectors, which never interrupts.
///
/// The count is the list's when it has one, else 1 (a device in message mode has
/// at least message 0), and never more than the device's table entries. A device
/// without an MSI-X capability cannot do virtio messages at all: 0, the INTx
/// path, whatever else is reported. Neither signal: 0.
pub const fn planned_messages(msix_ctrl: Option<u16>, listed: u32) -> u32 {
    let Some(ctrl) = msix_ctrl else { return 0 };
    if !msix_enabled(ctrl) && listed == 0 {
        return 0;
    }
    let want = if listed == 0 { 1 } else { listed };
    let table = msix_table_size(ctrl);
    if want > table {
        table
    } else {
        want
    }
}

// ── What the ISR does with a message ─────────────────────────────────────────

/// Bit 31 of the published state word: message mode is live.
const STATE_ACTIVE: u32 = 1 << 31;

/// Encode the ISR's view of a [`Plan`]: 0 means INTx; otherwise bit 31 and the
/// config vector (low 16 bits, [`NO_VECTOR`] for none). `0` can never be a
/// message-mode state because bit 31 is set.
pub const fn isr_state(plan: &Plan) -> u32 {
    if !(plan.config != NO_VECTOR
        || plan.queue[0] != NO_VECTOR
        || plan.queue[1] != NO_VECTOR
        || plan.queue[2] != NO_VECTOR
        || plan.queue[3] != NO_VECTOR)
    {
        return 0;
    }
    STATE_ACTIVE | plan.config as u32
}

/// What the ISR should do for one interrupt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IsrRoute {
    /// Not in message mode: the INTx path (ISR-status read-to-clear) runs.
    Intx,
    /// The configuration-change message: latch it for the DPC, queue the DPC.
    Config,
    /// A queue message (or the shared one): queue the DPC.
    Queue,
}

/// Route a message under `state` (from [`isr_state`]).
pub const fn isr_route(state: u32, message: u32) -> IsrRoute {
    if state & STATE_ACTIVE == 0 {
        return IsrRoute::Intx;
    }
    let config = (state & 0xFFFF) as u16;
    if config != NO_VECTOR && message == config as u32 {
        IsrRoute::Config
    } else {
        IsrRoute::Queue
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::vec::Vec;

    #[test]
    fn plan_none_when_nothing_granted() {
        assert_eq!(plan(0, 2, false), Plan::NONE);
        assert!(!plan(0, 2, false).uses_messages());
        assert_eq!(isr_state(&plan(0, 2, false)), 0);
    }

    #[test]
    fn plan_one_message_shares_and_leaves_config_unassigned() {
        let p = plan(1, 2, false);
        assert_eq!(p.config, NO_VECTOR);
        assert_eq!(p.queue, [0, 0, NO_VECTOR, NO_VECTOR]);
        assert_eq!(p.max_vector(), Some(0));
    }

    #[test]
    fn plan_three_messages_is_config_then_one_per_queue() {
        let p = plan(3, 2, false);
        assert_eq!(p.config, 0);
        assert_eq!(p.queue, [1, 2, NO_VECTOR, NO_VECTOR]);
        assert_eq!(p.max_vector(), Some(2));
    }

    #[test]
    fn plan_two_messages_keeps_config_separate() {
        let p = plan(2, 2, false);
        assert_eq!(p.config, 0);
        assert_eq!(p.queue[..2], [1, 1]);
    }

    #[test]
    fn plan_extends_to_a_third_queue_without_a_rewrite() {
        let p = plan(4, 3, false);
        assert_eq!(p.config, 0);
        assert_eq!(p.queue, [1, 2, 3, NO_VECTOR]);
        // Not enough messages for it: the extra queue shares the last one.
        let p = plan(3, 3, false);
        assert_eq!(p.queue, [1, 2, 2, NO_VECTOR]);
    }

    #[test]
    fn plan_never_names_an_ungranted_vector() {
        for granted in 0..8u32 {
            for queues in 0..=MAX_QUEUES {
                for shared in [false, true] {
                    let p = plan(granted, queues, shared);
                    if let Some(max) = p.max_vector() {
                        assert!(
                            (max as u32) < granted,
                            "granted={granted} queues={queues} shared={shared}: {p:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn plan_shared_only_forces_one_message() {
        let p = plan(3, 2, true);
        assert_eq!(p, Plan::all_shared(2));
        assert_eq!(p.queue[..2], [0, 0]);
    }

    #[test]
    fn plan_ignores_queues_past_the_table() {
        let p = plan(8, 9, false);
        assert_eq!(p.queue, [1, 2, 3, 4]);
    }

    #[test]
    fn readback() {
        assert!(vector_accepted(2, 2));
        assert!(!vector_accepted(2, NO_VECTOR));
        assert!(vector_accepted(NO_VECTOR, NO_VECTOR));
    }

    #[test]
    fn capability_bits() {
        assert!(msix_enabled(0x8002));
        assert!(!msix_enabled(0x0002));
        assert_eq!(msix_table_size(0x8002), 3);
        assert_eq!(msix_table_size(0x0000), 1);
        assert_eq!(msix_table_size(0x87FF), 0x800);
    }

    fn list(descs: &[(u8, u16)]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&1u32.to_le_bytes()); // full descriptors
        b.extend_from_slice(&5u32.to_le_bytes()); // InterfaceType
        b.extend_from_slice(&0u32.to_le_bytes()); // BusNumber
        b.extend_from_slice(&[1, 0, 1, 0]); // Version, Revision
        b.extend_from_slice(&(descs.len() as u32).to_le_bytes());
        for &(ty, flags) in descs {
            b.push(ty);
            b.push(1);
            b.extend_from_slice(&flags.to_le_bytes());
            b.extend_from_slice(&[0u8; 16]);
        }
        b
    }

    fn rd(b: &[u8]) -> impl Fn(usize) -> Option<u32> + '_ {
        move |off| {
            let w = b.get(off..off + 4)?;
            Some(u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
        }
    }

    #[test]
    fn resource_list_counts_message_interrupt_descriptors() {
        // memory, memory, three message interrupts.
        let l = list(&[(3, 0), (3, 0), (2, 2), (2, 2), (2, 2)]);
        assert_eq!(granted_messages(rd(&l)), 3);
        // a line-based interrupt is not a message.
        let l = list(&[(3, 0), (2, 1)]);
        assert_eq!(granted_messages(rd(&l)), 0);
        // one descriptor (count carried inside it).
        let l = list(&[(3, 0), (2, 2)]);
        assert_eq!(granted_messages(rd(&l)), 1);
        // policy-included flag next to the message flag.
        let l = list(&[(2, 2 | 4)]);
        assert_eq!(granted_messages(rd(&l)), 1);
    }

    #[test]
    fn resource_list_garbage_is_a_lower_bound_never_a_crash() {
        assert_eq!(granted_messages(rd(&[])), 0);
        assert_eq!(granted_messages(rd(&0u32.to_le_bytes())), 0);
        // Count claims 1000 descriptors, buffer holds one: reads stop at the end.
        let mut l = list(&[(2, 2)]);
        l[16..20].copy_from_slice(&1000u32.to_le_bytes());
        assert_eq!(granted_messages(rd(&l)), 1);
        // A device-specific blob ends the walk.
        let l = list(&[(2, 2), (5, 0), (2, 2)]);
        assert_eq!(granted_messages(rd(&l)), 1);
    }

    #[test]
    fn planned_messages_takes_either_signal() {
        // Enable bit set, 3 table entries, 3 listed.
        assert_eq!(planned_messages(Some(0x8002), 3), 3);
        // Neither signal: INTx.
        assert_eq!(planned_messages(Some(0x0002), 0), 0);
        // Enable bit set but the list could not be parsed: one message exists.
        assert_eq!(planned_messages(Some(0x8002), 0), 1);
        // Messages listed but the Enable bit not (yet) set: still messages.
        assert_eq!(planned_messages(Some(0x0002), 3), 3);
        // More listed than the table has: the table wins.
        assert_eq!(planned_messages(Some(0x8001), 5), 2);
        // No MSI-X capability: never messages.
        assert_eq!(planned_messages(None, 3), 0);
        assert_eq!(planned_messages(None, 0), 0);
    }

    #[test]
    fn isr_routes() {
        assert_eq!(isr_route(0, 0), IsrRoute::Intx);
        assert_eq!(isr_route(0, 7), IsrRoute::Intx);
        let st = isr_state(&plan(3, 2, false));
        assert_ne!(st, 0);
        assert_eq!(isr_route(st, 0), IsrRoute::Config);
        assert_eq!(isr_route(st, 1), IsrRoute::Queue);
        assert_eq!(isr_route(st, 2), IsrRoute::Queue);
        // Shared message 0: nothing is config.
        let st = isr_state(&plan(1, 2, false));
        assert_ne!(st, 0);
        assert_eq!(isr_route(st, 0), IsrRoute::Queue);
        // A plan with no message at all is INTx.
        assert_eq!(isr_state(&Plan::NONE), 0);
    }
}
