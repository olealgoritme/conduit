//! Completion hints (`HELIOS_ESCAPE_ECL_DONE`, `Nvk12CompletionHint`): which named
//! fence the KMD fires on a process's word that its batch finished
//! (`guest/windows/docs/rm-fence-marker.md`, "Completion hints").
//!
//! The D3D12 UMD's engine worker reads a batch's completion timeline in the guest and
//! names the batch's ECL fence as soon as it passed, ahead of the host's
//! `EventReady`. The KMD then fires that fence exactly as the first `EventReady`
//! would (the gate point fires, the packet's DMA completion is released by the
//! next DPC). The only check is ownership: the fence must be one the KMD holds for
//! the caller's own RM gate. So a process can complete only its own packets early.
//!
//! Pure functions over the per-handle state (`rm_fence_present::FenceMeta`) and the
//! gate's process, so the rule is host-tested; the KMD supplies the state under
//! `virtio_lock`.

use crate::rm_fence_present::{same_process, Attach, FenceMeta};

/// What the KMD knows of a named handle, read under `virtio_lock`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Named {
    /// The handle table entry is a fence (`DEVICE_TYPE_FENCE`).
    pub is_fence: bool,
    /// The KMD owns it (`DeviceOwner::KMD_RM`): a carrier took it over.
    pub kmd_owned: bool,
    /// The fence's state.
    pub meta: FenceMeta,
    /// When the fence is attached to a gate that is in use: the gate's process.
    pub gate_process: Option<usize>,
}

/// Why a named handle was not fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// No such handle (closed already, or never one).
    Unknown = 1,
    /// Not a fence.
    NotFence = 2,
    /// A fence the KMD does not hold: no carrier took it yet (its HE12 v4 Render has
    /// not run) or it is still its creator's.
    NotTaken = 3,
    /// Held by the KMD for something other than a gate point (a scanout flip, a
    /// discarded present tail).
    NotGate = 4,
    /// Another process's fence, or its gate is gone or belongs to another process.
    NotYours = 5,
}

/// The decision for one named handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Fire it now: it is the caller's, attached to gate `.0`, and has not fired.
    Fire(u8),
    /// The caller's, but it fired already (the host's `EventReady` or an earlier
    /// hint was first).
    Late,
    /// Not fired; see [`Refused`].
    Refused(Refused),
}

/// The verdict for a handle named by process `caller` (its `hKmdProcess`; 0 never
/// matches). `named` is `None` when the handle is not in the table.
pub fn verdict(named: Option<Named>, caller: usize) -> Verdict {
    let Some(n) = named else {
        return Verdict::Refused(Refused::Unknown);
    };
    if !n.is_fence {
        return Verdict::Refused(Refused::NotFence);
    }
    if !n.kmd_owned {
        return Verdict::Refused(Refused::NotTaken);
    }
    let Attach::Gate(gate) = n.meta.attached() else {
        return Verdict::Refused(Refused::NotGate);
    };
    if !same_process(n.meta.process(), caller)
        || !n.gate_process.is_some_and(|p| same_process(p, caller))
    {
        return Verdict::Refused(Refused::NotYours);
    }
    if n.meta.fired().is_some() {
        return Verdict::Late;
    }
    Verdict::Fire(gate)
}

/// What one escape did, for its out fields and the counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tally {
    pub fired: u32,
    pub late: u32,
    pub refused: u32,
    /// The last refusal's reason (`Refused as u32`), 0 when none.
    pub last_why: u32,
}

impl Tally {
    pub fn note(&mut self, v: Verdict) {
        match v {
            Verdict::Fire(_) => self.fired += 1,
            Verdict::Late => self.late += 1,
            Verdict::Refused(why) => {
                self.refused += 1;
                self.last_why = why as u32;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: usize = 0x1000;
    const OTHER: usize = 0x2000;

    fn taken(process: usize, gate: u8, gate_process: Option<usize>) -> Named {
        let mut meta = FenceMeta::new(process);
        meta.attach(Attach::Gate(gate)).unwrap();
        Named {
            is_fence: true,
            kmd_owned: true,
            meta,
            gate_process,
        }
    }

    #[test]
    fn own_attached_unfired_fence_fires() {
        assert_eq!(verdict(Some(taken(ME, 3, Some(ME))), ME), Verdict::Fire(3));
    }

    #[test]
    fn a_fired_fence_is_late() {
        let mut n = taken(ME, 3, Some(ME));
        assert!(n.meta.set_fired(0));
        assert_eq!(verdict(Some(n), ME), Verdict::Late);
        // An error fire is still a fire.
        let mut n = taken(ME, 3, Some(ME));
        assert!(n.meta.set_fired(-110));
        assert_eq!(verdict(Some(n), ME), Verdict::Late);
    }

    #[test]
    fn unknown_and_non_fences_are_refused() {
        assert_eq!(verdict(None, ME), Verdict::Refused(Refused::Unknown));
        let mut n = taken(ME, 0, Some(ME));
        n.is_fence = false;
        assert_eq!(verdict(Some(n), ME), Verdict::Refused(Refused::NotFence));
    }

    #[test]
    fn a_fence_not_taken_yet_is_refused() {
        // Created by the caller, its Render has not attached it: the host's
        // EventReady (or the early-fire latch at attach) covers it.
        let n = Named {
            is_fence: true,
            kmd_owned: false,
            meta: FenceMeta::new(ME),
            gate_process: None,
        };
        assert_eq!(verdict(Some(n), ME), Verdict::Refused(Refused::NotTaken));
    }

    #[test]
    fn scanout_and_discarded_fences_are_refused() {
        for to in [Attach::Scanout, Attach::Discard] {
            let mut meta = FenceMeta::new(ME);
            meta.attach(to).unwrap();
            let n = Named {
                is_fence: true,
                kmd_owned: true,
                meta,
                gate_process: None,
            };
            assert_eq!(verdict(Some(n), ME), Verdict::Refused(Refused::NotGate));
        }
    }

    #[test]
    fn another_process_cannot_fire_a_fence() {
        assert_eq!(
            verdict(Some(taken(OTHER, 1, Some(OTHER))), ME),
            Verdict::Refused(Refused::NotYours)
        );
        // Even one that already fired: nothing is learnt of others' handles.
        let mut n = taken(OTHER, 1, Some(OTHER));
        n.meta.set_fired(0);
        assert_eq!(verdict(Some(n), ME), Verdict::Refused(Refused::NotYours));
    }

    #[test]
    fn a_purged_or_foreign_gate_is_refused() {
        assert_eq!(
            verdict(Some(taken(ME, 1, None)), ME),
            Verdict::Refused(Refused::NotYours)
        );
        assert_eq!(
            verdict(Some(taken(ME, 1, Some(OTHER))), ME),
            Verdict::Refused(Refused::NotYours)
        );
    }

    #[test]
    fn process_zero_never_matches() {
        assert_eq!(
            verdict(Some(taken(0, 1, Some(0))), 0),
            Verdict::Refused(Refused::NotYours)
        );
    }

    #[test]
    fn tally_counts_each_outcome() {
        let mut t = Tally::default();
        t.note(Verdict::Fire(0));
        t.note(Verdict::Fire(1));
        t.note(Verdict::Late);
        t.note(Verdict::Refused(Refused::NotTaken));
        t.note(Verdict::Refused(Refused::Unknown));
        assert_eq!(
            t,
            Tally {
                fired: 2,
                late: 1,
                refused: 2,
                last_why: Refused::Unknown as u32
            }
        );
    }
}
