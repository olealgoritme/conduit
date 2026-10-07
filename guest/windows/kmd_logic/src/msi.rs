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

/// The ISR state of a start that got messages but runs with NO vector programmed
/// (the device refused every plan): message mode, config unassigned, so any stray
/// message is queue work. Never 0. Nothing is expected to fire; completions are
/// found by polling (`virtio::msi`).
pub const fn polling_only_state() -> u32 {
    STATE_ACTIVE | NO_VECTOR as u32
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

// ── Setup: what to try when the device refuses a vector ──────────────────────

/// The plan to try after `refusals` refused attempts, or `None` when the next
/// step is to give up on messages for this start.
///
/// * attempt 0: the plan [`plan`] gives (per-queue vectors when enough messages
///   were granted);
/// * attempt 1: every queue on message 0, unless attempt 0 already was that (the
///   same writes cannot succeed the second time, and the retry is skipped);
/// * later: give up.
///
/// Giving up is NOT a switch to INTx: the OS connected messages and not the
/// line, so the device would never be heard. The caller fails the transport
/// (the adapter starts render-only) and latches INTx for the NEXT start
/// ([`key_action`]).
pub fn setup_plan(refusals: u32, granted: u32, queues: usize, shared_only: bool) -> Option<Plan> {
    let first = plan(granted, queues, shared_only);
    match refusals {
        0 => {
            if first.uses_messages() {
                Some(first)
            } else {
                None
            }
        }
        1 => {
            let shared = Plan::all_shared(queues);
            if !first.uses_messages() || plan_eq(&first, &shared) {
                None
            } else {
                Some(shared)
            }
        }
        _ => None,
    }
}

const fn plan_eq(a: &Plan, b: &Plan) -> bool {
    if a.config != b.config {
        return false;
    }
    let mut i = 0;
    while i < MAX_QUEUES {
        if a.queue[i] != b.queue[i] {
            return false;
        }
        i += 1;
    }
    true
}

// ── Policy: what the device key should ask PnP for ───────────────────────────

/// The service-key knob `MsiMode`: which interrupt mode the driver asks PnP for.
/// PnP decides from the device key's `MSISupported` before `StartDevice`, so
/// this is realised by writing that value (see [`key_action`]), which takes
/// effect at the NEXT device start: the first restart after a change writes the
/// key, a second restart (or a reboot) applies it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// 0 (default): follow the INF / the device key as it stands. The driver
    /// only ever LOWERS it on its own (a latched start).
    Auto,
    /// 1: INTx always (the A/B and the escape hatch). Survives a driver update.
    ForceIntx,
    /// 2: MSI-X (writes the key to 1), but the breaker and the latch still win:
    /// a start that never became healthy, or a convicted delivery, puts INTx back.
    ForceMsi,
    /// 3: MSI-X, no breaker and no latch: debugging only.
    ForceMsiNoBreaker,
}

impl Mode {
    /// The knob's value. Anything unknown is `Auto`: a typo must not pick a mode.
    pub const fn from_knob(value: u32) -> Mode {
        match value {
            1 => Mode::ForceIntx,
            2 => Mode::ForceMsi,
            3 => Mode::ForceMsiNoBreaker,
            _ => Mode::Auto,
        }
    }

    /// The knob's value for this mode (the mirror `MsiModeEff`).
    pub const fn code(self) -> u32 {
        match self {
            Mode::Auto => 0,
            Mode::ForceIntx => 1,
            Mode::ForceMsi => 2,
            Mode::ForceMsiNoBreaker => 3,
        }
    }
}

/// What the KMD does to the device key's `MSISupported` at `AddDevice`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyAction {
    /// Do not touch it: the INF's value (or a hand-set one) stands.
    Leave,
    /// Write 0: the device comes up on the INTx line.
    SetIntx,
    /// Write 1: the device comes up on messages.
    SetMsi,
}

impl KeyAction {
    /// The `MSISupported` value to write, if any.
    pub const fn value(self) -> Option<u32> {
        match self {
            KeyAction::Leave => None,
            KeyAction::SetIntx => Some(0),
            KeyAction::SetMsi => Some(1),
        }
    }

    /// The `MsiWant` mirror: 0xFF = left alone, else the value written.
    pub const fn mirror(self) -> u32 {
        match self.value() {
            None => 0xFF,
            Some(v) => v,
        }
    }
}

/// The key action for `mode` and whether INTx is latched (an earlier start's
/// conviction, or the boot-loop breaker, see [`breaker_trips`]).
///
/// The shipped INF value (INTx in this package) is the single source of truth:
/// `Auto` leaves the key alone, except that a latch lowers it (lowering is always
/// safe). `ForceMsi` raises it, but never over a latch; only `ForceMsiNoBreaker`
/// does. `ForceIntx` always lowers it. After a change of `MsiMode` the first
/// restart writes the key; the second applies it. Going back to `Auto` leaves the
/// key where the last forcing put it, until the next package install rewrites it
/// from the INF.
pub const fn key_action(mode: Mode, latched: bool) -> KeyAction {
    match mode {
        Mode::ForceIntx => KeyAction::SetIntx,
        Mode::ForceMsiNoBreaker => KeyAction::SetMsi,
        Mode::ForceMsi | Mode::Auto => {
            if latched {
                KeyAction::SetIntx
            } else if matches!(mode, Mode::ForceMsi) {
                KeyAction::SetMsi
            } else {
                KeyAction::Leave
            }
        }
    }
}

/// The boot-loop breaker. A start in message mode sets the `MsiStarting` marker
/// (flushed to disk) and clears it once interrupts are seen to arrive
/// ([`marker_may_clear`]). `AddDevice` finding the marker still set means the
/// previous message-mode start never became healthy (a hang, a bugcheck, a
/// reboot into the same fault): that trips the breaker, which latches INTx.
/// `ForceMsiNoBreaker` is the only mode that does not act on it.
pub const fn breaker_trips(mode: Mode, marker_found: bool) -> bool {
    marker_found && !matches!(mode, Mode::ForceMsiNoBreaker)
}

/// How long after a start finished the marker waits before it may be cleared
/// (3 s, 100 ns units): a start that dies within seconds of looking healthy
/// still trips the breaker.
pub const MARKER_CLEAR_AFTER_100NS: u64 = 30_000_000;

/// Whether the `MsiStarting` marker may be cleared now: run-time judging is armed
/// (`armed_at` is its time, 0 = not armed), delivery is not convicted, at least one
/// interrupt arrived, and either the start is ending cleanly (`stopping`) or
/// [`MARKER_CLEAR_AFTER_100NS`] has passed.
pub const fn marker_may_clear(
    armed_at: u64,
    now: u64,
    ints_seen: u32,
    convicted: bool,
    stopping: bool,
) -> bool {
    armed_at != 0
        && !convicted
        && ints_seen > 0
        && (stopping || now.saturating_sub(armed_at) >= MARKER_CLEAR_AFTER_100NS)
}

// ── Counting: per-vector interrupts and DPCs ─────────────────────────────────

/// Messages counted by their own slot (`MsiV0`..`MsiV3`); a higher one shares
/// [`SLOT_OTHER`].
pub const COUNTED_VECTORS: usize = 4;
/// The shared slot for message numbers above the counted ones.
pub const SLOT_OTHER: usize = COUNTED_VECTORS;
/// Slots in the per-vector arrays.
pub const SLOTS: usize = COUNTED_VECTORS + 1;

/// The counter slot for a message number.
pub const fn vector_slot(message: u32) -> usize {
    if (message as usize) < COUNTED_VECTORS {
        message as usize
    } else {
        SLOT_OTHER
    }
}

/// DPC cause bit of the INTx path.
pub const CAUSE_INTX: u32 = 1 << SLOTS;

/// The cause bit a message sets for the DPC (`1 << slot`).
pub const fn cause_bit(message: u32) -> u32 {
    1 << vector_slot(message)
}

/// Whether `cause` (the mask the DPC took) includes message slot `slot`.
pub const fn cause_has_slot(cause: u32, slot: usize) -> bool {
    slot < SLOTS && cause & (1 << slot) != 0
}

// ── Health: is message delivery working? ─────────────────────────────────────

/// Silent rescues in a row that convict message delivery (see [`rescue_step`]).
pub const SILENT_RESCUES_BROKEN: u32 = 3;
/// Completions a start must have seen before silence means anything.
pub const START_MIN_COMPLETIONS: u32 = 3;

/// What is known about message delivery. The order is the severity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Health {
    /// Nothing to judge from yet. The state of INTx mode, always.
    Unknown = 0,
    /// Interrupts arrive.
    Healthy = 1,
    /// Completions were found by polling with no interrupt for them.
    Suspect = 2,
    /// Enough of that in a row: delivery does not work.
    Broken = 3,
}

impl Health {
    pub const fn code(self) -> u32 {
        self as u32
    }

    /// The state a stored [`Health::code`] names; anything unknown is `Unknown`.
    pub const fn from_code(code: u32) -> Health {
        match code {
            1 => Health::Healthy,
            2 => Health::Suspect,
            3 => Health::Broken,
            _ => Health::Unknown,
        }
    }
}

/// A suspect state is cleared once interrupts have arrived since the evidence
/// that raised it (`ints_prev`: the total at the last rescue, or when run-time
/// judging was armed): the silence was the start, not the delivery. `Broken` is a
/// conviction and stays, and so does everything else.
pub const fn reassess(health: Health, ints_prev: u32, ints_now: u32) -> Health {
    if matches!(health, Health::Suspect) && ints_now != ints_prev {
        Health::Healthy
    } else {
        health
    }
}

/// The outcome of one rescue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RescueStep {
    /// Silent rescues in a row, after this one.
    pub streak: u32,
    pub health: Health,
}

/// One RESCUE: a polling drain (after a wait slice timed out) found a completion
/// the interrupt had not delivered. `ints_prev` is the interrupt total at the
/// previous rescue (or when health was armed), `ints_now` the total now.
///
/// If interrupts arrived in between, delivery works: the streak ends. A rescue
/// with no interrupt since the last one is SILENT; [`SILENT_RESCUES_BROKEN`] of
/// them in a row is `Broken`. The comparison is `!=`, so a wrapped total is
/// still "moved".
pub const fn rescue_step(streak: u32, ints_prev: u32, ints_now: u32) -> RescueStep {
    if ints_now != ints_prev {
        return RescueStep {
            streak: 0,
            health: Health::Healthy,
        };
    }
    let streak = streak.saturating_add(1);
    RescueStep {
        streak,
        health: if streak >= SILENT_RESCUES_BROKEN {
            Health::Broken
        } else {
            Health::Suspect
        },
    }
}

/// The verdict at the end of `StartDevice` from the interrupts taken and the
/// completions popped since the transport went live.
///
/// Never `Broken`: whether dxgkrnl delivers interrupts before `StartDevice`
/// returns is a hardware question this table cannot settle, and a false
/// conviction would turn the default off for good. `Suspect` only turns the
/// polling safety net on, which is harmless.
pub const fn start_verdict(ints: u32, completions: u32) -> Health {
    if ints > 0 {
        Health::Healthy
    } else if completions >= START_MIN_COMPLETIONS {
        Health::Suspect
    } else {
        Health::Unknown
    }
}

/// Whether the worker should poll the used rings (the safety net for a lost
/// interrupt): a start that latched INTx and still got messages, or delivery
/// that is suspect or broken.
pub const fn polling_wanted(latched: bool, health: Health) -> bool {
    latched || matches!(health, Health::Suspect | Health::Broken)
}

/// Whether to latch INTx for the next start: only a conviction.
pub const fn should_latch(health: Health) -> bool {
    matches!(health, Health::Broken)
}

/// Why INTx was latched (`MsiLatchWhy`).
pub mod latch_why {
    /// Silent rescues in a row at run time.
    pub const SILENT: u32 = 1;
    /// The device refused every vector plan.
    pub const REFUSED: u32 = 2;
    /// The common configuration could not be mapped.
    pub const NO_CFG: u32 = 3;
    /// The boot-loop breaker: the previous message-mode start never became healthy.
    pub const BREAKER: u32 = 4;
}

/// Service-key value names of the message-interrupt counters, written by
/// `kmd_render/src/virtio/msi.rs` only. `MsiMode` and `MsiVectors` are knobs the
/// operator writes and are not listed; `MsiLatch` is both: the KMD writes 1, an
/// operator may write either. At most 14 characters each.
pub const COUNTERS: [&str; 36] = [
    // The set-up breadcrumbs.
    "MsiCap",
    "MsiList",
    "MsiGrant",
    "MsiVec",
    "MsiRefused",
    "MsiNoCfg",
    // Interrupts: the total and per message number.
    "MsiInts",
    "MsiV0",
    "MsiV1",
    "MsiV2",
    "MsiV3",
    "MsiVOth",
    // DPCs, by what queued them.
    "MsiDpc0",
    "MsiDpc1",
    "MsiDpc2",
    "MsiDpc3",
    "MsiDpcOth",
    "IntxDpc",
    "DpcNoCause",
    // The INTx path.
    "IntxInts",
    "IntxMiss",
    // Delivery health.
    "MsiIdle",
    "IrqRescue",
    "MsiSilent",
    "MsiHealth",
    "MsiStart",
    "MsiPoll",
    "MsiPollN",
    "MsiPollOnly",
    // Policy and the latch.
    "MsiModeEff",
    "MsiLatch",
    "MsiLatchWhy",
    "MsiWant",
    "MsiKeyWr",
    // The boot-loop breaker: the marker a message-mode start sets, how often it tripped.
    "MsiStarting",
    "MsiBreaker",
];

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

    // ── setup fallback ───────────────────────────────────────────────────────

    #[test]
    fn setup_tries_the_plan_then_shared_then_gives_up() {
        // Three messages: per-queue first, then everything on message 0, then out.
        assert_eq!(setup_plan(0, 3, 2, false), Some(plan(3, 2, false)));
        assert_eq!(setup_plan(1, 3, 2, false), Some(Plan::all_shared(2)));
        assert_eq!(setup_plan(2, 3, 2, false), None);
        assert_eq!(setup_plan(9, 3, 2, false), None);
    }

    #[test]
    fn setup_does_not_retry_the_same_writes() {
        // One message, or the shared knob: attempt 0 already is the shared plan.
        assert_eq!(setup_plan(0, 1, 2, false), Some(Plan::all_shared(2)));
        assert_eq!(setup_plan(1, 1, 2, false), None);
        assert_eq!(setup_plan(0, 3, 2, true), Some(Plan::all_shared(2)));
        assert_eq!(setup_plan(1, 3, 2, true), None);
        // Two messages with two queues is per-queue (config 0, both on 1): not shared.
        assert_eq!(setup_plan(1, 2, 2, false), Some(Plan::all_shared(2)));
    }

    #[test]
    fn setup_without_messages_has_no_plan() {
        for refusals in 0..4 {
            assert_eq!(setup_plan(refusals, 0, 2, false), None);
        }
    }

    #[test]
    fn setup_never_names_an_ungranted_vector_on_any_attempt() {
        for granted in 0..8u32 {
            for queues in 0..=MAX_QUEUES {
                for shared in [false, true] {
                    for refusals in 0..3u32 {
                        if let Some(p) = setup_plan(refusals, granted, queues, shared) {
                            if let Some(max) = p.max_vector() {
                                assert!((max as u32) < granted);
                            }
                        }
                    }
                }
            }
        }
    }

    // ── policy ───────────────────────────────────────────────────────────────

    #[test]
    fn mode_knob_values() {
        assert_eq!(Mode::from_knob(0), Mode::Auto);
        assert_eq!(Mode::from_knob(1), Mode::ForceIntx);
        assert_eq!(Mode::from_knob(2), Mode::ForceMsi);
        assert_eq!(Mode::from_knob(3), Mode::ForceMsiNoBreaker);
        // Unknown values do not pick a mode.
        assert_eq!(Mode::from_knob(4), Mode::Auto);
        assert_eq!(Mode::from_knob(u32::MAX), Mode::Auto);
        for m in [
            Mode::Auto,
            Mode::ForceIntx,
            Mode::ForceMsi,
            Mode::ForceMsiNoBreaker,
        ] {
            assert_eq!(Mode::from_knob(m.code()), m);
        }
    }

    #[test]
    fn key_action_table() {
        // Default, nothing latched: the INF's value stands (INTx in this package).
        assert_eq!(key_action(Mode::Auto, false), KeyAction::Leave);
        // A latch lowers the key whatever the mode asks, except the debugging mode.
        assert_eq!(key_action(Mode::Auto, true), KeyAction::SetIntx);
        assert_eq!(key_action(Mode::ForceMsi, true), KeyAction::SetIntx);
        assert_eq!(key_action(Mode::ForceIntx, false), KeyAction::SetIntx);
        assert_eq!(key_action(Mode::ForceIntx, true), KeyAction::SetIntx);
        // The opt-in raises the key.
        assert_eq!(key_action(Mode::ForceMsi, false), KeyAction::SetMsi);
        assert_eq!(
            key_action(Mode::ForceMsiNoBreaker, false),
            KeyAction::SetMsi
        );
        assert_eq!(key_action(Mode::ForceMsiNoBreaker, true), KeyAction::SetMsi);
    }

    #[test]
    fn only_an_explicit_opt_in_raises_the_key() {
        // Raising MSISupported without being asked is the INF's job, never the KMD's.
        for latched in [false, true] {
            assert_ne!(key_action(Mode::Auto, latched), KeyAction::SetMsi);
            assert_ne!(key_action(Mode::ForceIntx, latched), KeyAction::SetMsi);
        }
        // And a latch beats everything but mode 3.
        for mode in [Mode::Auto, Mode::ForceIntx, Mode::ForceMsi] {
            assert_ne!(key_action(mode, true), KeyAction::SetMsi);
        }
    }

    #[test]
    fn the_breaker_trips_on_a_leftover_marker_in_every_mode_but_the_debugging_one() {
        for mode in [Mode::Auto, Mode::ForceIntx, Mode::ForceMsi] {
            assert!(breaker_trips(mode, true));
            assert!(!breaker_trips(mode, false));
        }
        assert!(!breaker_trips(Mode::ForceMsiNoBreaker, true));
        assert!(!breaker_trips(Mode::ForceMsiNoBreaker, false));
        // A tripped breaker is a latch, and a latch lowers an opted-in key.
        let latched = breaker_trips(Mode::ForceMsi, true);
        assert_eq!(key_action(Mode::ForceMsi, latched), KeyAction::SetIntx);
    }

    #[test]
    fn the_marker_clears_only_when_interrupts_were_seen_and_the_start_is_old_enough() {
        let armed = 1_000_000u64;
        let after = armed + MARKER_CLEAR_AFTER_100NS;
        // Not armed yet: never.
        assert!(!marker_may_clear(0, after, 5, false, false));
        assert!(!marker_may_clear(0, after, 5, false, true));
        // Armed, interrupts seen, old enough.
        assert!(marker_may_clear(armed, after, 1, false, false));
        // Too young, unless the start is ending cleanly.
        assert!(!marker_may_clear(armed, after - 1, 1, false, false));
        assert!(marker_may_clear(armed, armed, 1, false, true));
        // No interrupt: a quiet device is not a healthy one, even stopping.
        assert!(!marker_may_clear(armed, after, 0, false, false));
        assert!(!marker_may_clear(armed, after, 0, false, true));
        // A convicted delivery keeps the marker.
        assert!(!marker_may_clear(armed, after, 9, true, false));
        assert!(!marker_may_clear(armed, after, 9, true, true));
        // A clock that went backwards does not clear early.
        assert!(!marker_may_clear(armed, 0, 1, false, false));
    }

    #[test]
    fn the_polling_only_state_is_message_mode_with_nothing_to_route() {
        let st = polling_only_state();
        assert_ne!(st, 0);
        for m in [0, 1, 2, 3, 40] {
            // No config vector: every message is queue work, never a config change.
            assert_eq!(isr_route(st, m), IsrRoute::Queue);
        }
        // It is not what a plan with a vector encodes.
        assert_ne!(st, isr_state(&plan(3, 2, false)));
    }

    #[test]
    fn key_action_values() {
        assert_eq!(KeyAction::Leave.value(), None);
        assert_eq!(KeyAction::SetIntx.value(), Some(0));
        assert_eq!(KeyAction::SetMsi.value(), Some(1));
        assert_eq!(KeyAction::Leave.mirror(), 0xFF);
        assert_eq!(KeyAction::SetIntx.mirror(), 0);
        assert_eq!(KeyAction::SetMsi.mirror(), 1);
    }

    // ── per-vector counting ──────────────────────────────────────────────────

    #[test]
    fn vector_slots() {
        for m in 0..COUNTED_VECTORS as u32 {
            assert_eq!(vector_slot(m), m as usize);
        }
        assert_eq!(vector_slot(COUNTED_VECTORS as u32), SLOT_OTHER);
        assert_eq!(vector_slot(2047), SLOT_OTHER);
        assert_eq!(vector_slot(u32::MAX), SLOT_OTHER);
        assert!(vector_slot(u32::MAX) < SLOTS);
    }

    #[test]
    fn cause_bits_are_distinct_and_fit() {
        let mut seen = 0u32;
        for m in 0..=COUNTED_VECTORS as u32 {
            let b = cause_bit(m);
            assert_eq!(b.count_ones(), 1);
            assert_eq!(seen & b, 0, "message {m} shares a bit");
            seen |= b;
        }
        // INTx has its own bit, above every message slot.
        assert_eq!(CAUSE_INTX.count_ones(), 1);
        assert_eq!(seen & CAUSE_INTX, 0);
        // Every message above the counted ones lands on the shared bit.
        assert_eq!(cause_bit(40), cause_bit(COUNTED_VECTORS as u32));
    }

    #[test]
    fn cause_slots_round_trip() {
        let mask = cause_bit(0) | cause_bit(2) | CAUSE_INTX;
        assert!(cause_has_slot(mask, 0));
        assert!(!cause_has_slot(mask, 1));
        assert!(cause_has_slot(mask, 2));
        assert!(!cause_has_slot(mask, SLOT_OTHER));
        // INTx is not a message slot.
        for s in 0..SLOTS {
            assert_eq!(cause_has_slot(CAUSE_INTX, s), false);
        }
        // A slot past the table is never set.
        assert!(!cause_has_slot(u32::MAX, SLOTS));
        assert!(!cause_has_slot(u32::MAX, 31));
    }

    #[test]
    fn a_planned_vector_always_has_its_own_or_the_other_slot() {
        // Every vector any plan uses is counted somewhere.
        for granted in 0..8u32 {
            let p = plan(granted, 2, false);
            if let Some(max) = p.max_vector() {
                assert!(vector_slot(max as u32) < SLOTS);
            }
        }
    }

    // ── health ───────────────────────────────────────────────────────────────

    #[test]
    fn silent_rescues_convict_only_in_a_row() {
        let mut streak = 0;
        let mut health = Health::Unknown;
        for i in 1..=SILENT_RESCUES_BROKEN {
            let s = rescue_step(streak, 100, 100);
            streak = s.streak;
            health = s.health;
            assert_eq!(streak, i);
            if i < SILENT_RESCUES_BROKEN {
                assert_eq!(health, Health::Suspect);
            }
        }
        assert_eq!(health, Health::Broken);
        assert!(should_latch(health));
    }

    #[test]
    fn an_interrupt_between_rescues_ends_the_streak() {
        let s = rescue_step(2, 100, 101);
        assert_eq!(s.streak, 0);
        assert_eq!(s.health, Health::Healthy);
        assert!(!should_latch(s.health));
        // A wrapped total still counts as movement.
        assert_eq!(rescue_step(2, u32::MAX, 0).health, Health::Healthy);
        // And the next silent one starts again from one.
        assert_eq!(rescue_step(s.streak, 101, 101).streak, 1);
    }

    #[test]
    fn rescue_streak_saturates() {
        let s = rescue_step(u32::MAX, 5, 5);
        assert_eq!(s.streak, u32::MAX);
        assert_eq!(s.health, Health::Broken);
    }

    #[test]
    fn start_verdict_never_convicts() {
        assert_eq!(start_verdict(5, 0), Health::Healthy);
        assert_eq!(start_verdict(1, 100), Health::Healthy);
        // Silence with too few completions proves nothing.
        assert_eq!(start_verdict(0, 0), Health::Unknown);
        assert_eq!(start_verdict(0, START_MIN_COMPLETIONS - 1), Health::Unknown);
        // Silence with completions is suspect, never broken.
        assert_eq!(start_verdict(0, START_MIN_COMPLETIONS), Health::Suspect);
        assert_eq!(start_verdict(0, u32::MAX), Health::Suspect);
        for ints in [0, 1, 7] {
            for done in [0, 3, 1000] {
                assert!(!should_latch(start_verdict(ints, done)));
            }
        }
    }

    #[test]
    fn polling_net_is_on_when_delivery_is_doubtful() {
        assert!(!polling_wanted(false, Health::Unknown));
        assert!(!polling_wanted(false, Health::Healthy));
        assert!(polling_wanted(false, Health::Suspect));
        assert!(polling_wanted(false, Health::Broken));
        // A start that latched INTx and still got messages polls from the start.
        assert!(polling_wanted(true, Health::Unknown));
        assert!(polling_wanted(true, Health::Healthy));
    }

    #[test]
    fn a_suspect_start_clears_once_interrupts_flow() {
        assert_eq!(reassess(Health::Suspect, 5, 6), Health::Healthy);
        assert_eq!(reassess(Health::Suspect, 5, 5), Health::Suspect);
        // A wrapped total is movement.
        assert_eq!(reassess(Health::Suspect, u32::MAX, 0), Health::Healthy);
        // A conviction is never cleared by it, nor is anything else touched.
        assert_eq!(reassess(Health::Broken, 5, 600), Health::Broken);
        assert_eq!(reassess(Health::Healthy, 5, 6), Health::Healthy);
        assert_eq!(reassess(Health::Unknown, 5, 6), Health::Unknown);
        // Once cleared, the polling net is off (unless a latch holds it on).
        assert!(!polling_wanted(false, reassess(Health::Suspect, 0, 1)));
        assert!(polling_wanted(true, reassess(Health::Suspect, 0, 1)));
    }

    #[test]
    fn health_codes_round_trip() {
        for h in [
            Health::Unknown,
            Health::Healthy,
            Health::Suspect,
            Health::Broken,
        ] {
            assert_eq!(Health::from_code(h.code()), h);
        }
        assert_eq!(Health::from_code(99), Health::Unknown);
    }

    #[test]
    fn health_order_is_severity() {
        assert!(Health::Unknown < Health::Healthy);
        assert!(Health::Healthy < Health::Suspect);
        assert!(Health::Suspect < Health::Broken);
        assert_eq!(Health::Broken.code(), 3);
    }

    // ── names ────────────────────────────────────────────────────────────────

    /// Every `b"..."` literal in the Rust files under `root` (name, file).
    fn byte_literals(root: &std::path::Path) -> Vec<(std::string::String, std::path::PathBuf)> {
        let mut out = Vec::new();
        let mut stack = std::vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for e in std::fs::read_dir(&dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    let text = std::fs::read_to_string(&p).unwrap();
                    let mut rest = text.as_str();
                    while let Some(i) = rest.find("b\"") {
                        let before = rest[..i].chars().last();
                        let tail = &rest[i + 2..];
                        let Some(end) = tail.find('"') else { break };
                        if !before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
                            out.push((tail[..end].into(), p.clone()));
                        }
                        rest = &tail[end + 1..];
                    }
                }
            }
        }
        out
    }

    #[test]
    fn counter_names_fit_are_unique_and_collide_with_nothing_else() {
        use std::string::String;
        let mut all: Vec<&str> = COUNTERS.to_vec();
        all.extend(crate::nvrm_rtt::COUNTERS);
        for n in &all {
            assert!(n.len() <= 14, "{n} is longer than 14");
            assert!(n.chars().all(|c| c.is_ascii_alphanumeric()), "{n}");
        }
        let mut sorted: Vec<String> = all.iter().map(|s| (*s).into()).collect();
        sorted.sort();
        let before = sorted.len();
        sorted.dedup();
        assert_eq!(sorted.len(), before, "duplicate counter name");
        // None is in another list of this crate.
        for other in crate::foreign_flip::COUNTERS
            .iter()
            .chain(crate::flip_completion::COUNTERS.iter())
            .chain(crate::stall_diag::COUNTERS.iter())
        {
            assert!(!all.contains(other), "{other} collides");
        }
        // The knobs are not counters: the operator writes them.
        for knob in ["MsiMode", "MsiVectors"] {
            assert!(!all.contains(&knob), "{knob} is a knob");
        }

        // The render crate, when this crate sits beside it.
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let render = manifest.join("../kmd_render/src");
        if render.exists() {
            let lits = byte_literals(&render);
            assert!(lits.len() > 500, "scan found {} literals", lits.len());
            for (lit, file) in &lits {
                let name = file.file_name().unwrap().to_string_lossy().into_owned();
                for mine in &all {
                    let is_rtt = mine.starts_with("NvRtt");
                    if lit == mine {
                        let ok = if is_rtt {
                            name == "nvrm.rs"
                        } else {
                            // These are also read as knobs, declared with the others in diag.rs.
                            name == "msi.rs"
                                || (["MsiLatch", "MsiStarting", "MsiBreaker"].contains(mine)
                                    && name == "diag.rs")
                        };
                        assert!(ok, "{mine} is also spelled in {}", file.display());
                    }
                    // A longer literal the 14-character clamp truncates onto one of mine.
                    if lit.len() > 14 && lit[..14] == **mine {
                        panic!("{lit} in {} truncates onto {mine}", file.display());
                    }
                }
            }
            // The knobs are read in diag.rs and written nowhere.
            let knob_sites: Vec<_> = lits
                .iter()
                .filter(|(l, _)| l == "MsiMode" || l == "MsiVectors")
                .map(|(_, f)| f.file_name().unwrap().to_string_lossy().into_owned())
                .collect();
            assert!(
                knob_sites.iter().all(|f| f == "diag.rs"),
                "a knob is spelled outside diag.rs: {knob_sites:?}"
            );
            // What the writers spell is exactly the lists.
            let spelled = |file: &str| -> Vec<String> {
                let mut v: Vec<String> = lits
                    .iter()
                    .filter(|(_, f)| {
                        f.file_name().is_some_and(|n| n == file)
                            && f.to_string_lossy().contains("virtio")
                    })
                    .map(|(l, _)| l.clone())
                    .collect();
                v.sort();
                v.dedup();
                v
            };
            let mut mine: Vec<String> = COUNTERS.iter().map(|s| (*s).into()).collect();
            mine.sort();
            let in_msi: Vec<String> = spelled("msi.rs");
            for m in &mine {
                assert!(in_msi.contains(m), "virtio/msi.rs does not write {m}");
            }
            let mut rtt: Vec<String> = crate::nvrm_rtt::COUNTERS
                .iter()
                .map(|s| (*s).into())
                .collect();
            rtt.sort();
            let in_nvrm: Vec<String> = spelled("nvrm.rs");
            for m in &rtt {
                assert!(in_nvrm.contains(m), "virtio/nvrm.rs does not write {m}");
            }
        }
    }
}
