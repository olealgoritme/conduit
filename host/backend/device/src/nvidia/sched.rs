// SPDX-License-Identifier: Apache-2.0
//
// The guest-TSG timeslice policy (crate::gpu_sched), applied where the
// guest's TSGs are made: after each successful `KEPLER_CHANNEL_GROUP_A`
// allocation the backend sets the new TSG's timeslice on the same RM client
// and file, as its own call, and it clamps the guest's own SET_TIMESLICE
// requests to the policy.

use super::*;
use crate::gpu_sched::{CMD_GET_TIMESLICE, CMD_SET_TIMESLICE, GuestSched, TSG_CLASS};
use crate::power::{Health, Probe};

/// Most guest channel groups remembered for the health probe; past it the
/// oldest are forgotten (a guest makes a few per GPU process).
const MAX_PROBED_TSGS: usize = 4096;

/// What the policy did, for the log.
#[derive(Default, Debug)]
pub(super) struct TsgSchedStats {
    pub(super) tsgs: u64,
    pub(super) timeslice_set: u64,
    pub(super) timeslice_refused: u64,
    pub(super) guest_clamped: u64,
    /// The first result is logged; the rest are counted.
    logged_timeslice: bool,
}

impl TsgSchedStats {
    pub(super) fn report(&self, sched: &GuestSched) {
        if self.tsgs == 0 && self.guest_clamped == 0 {
            return;
        }
        log::info!(
            "guest GPU timeslice ({}): {} TSG(s); set {} refused {}; \
             guest SET_TIMESLICE clamped {}",
            sched.describe(),
            self.tsgs,
            self.timeslice_set,
            self.timeslice_refused,
            self.guest_clamped
        );
    }
}

/// An NVOS54 control block addressing `params`. The pointer in it is only
/// valid while `params` is.
fn nvos54(client: u32, object: u32, cmd: u32, params: &mut [u8]) -> [u8; NVOS54_TOTAL] {
    let mut b = [0u8; NVOS54_TOTAL];
    b[0..4].copy_from_slice(&client.to_le_bytes());
    b[4..8].copy_from_slice(&object.to_le_bytes());
    b[NVOS54_CMD..NVOS54_CMD + 4].copy_from_slice(&cmd.to_le_bytes());
    b[16..24].copy_from_slice(&(params.as_mut_ptr() as u64).to_le_bytes());
    b[NVOS54_PARAMS_SIZE..NVOS54_PARAMS_SIZE + 4]
        .copy_from_slice(&(params.len() as u32).to_le_bytes());
    b
}

impl NvidiaBackend {
    /// One RM control of the backend's own on `fd`. The RM status, or the
    /// errno as `Err`.
    fn own_rm_control(
        &self,
        fd: RawFd,
        client: u32,
        object: u32,
        cmd: u32,
        params: &mut [u8],
    ) -> std::result::Result<u32, i32> {
        let mut outer = nvos54(client, object, cmd, params);
        let request = abi::ioctl::_IOWR(abi::ioctl::NV_ESC_RM_CONTROL, NVOS54_TOTAL as u32);
        self.host.ioctl(fd, request, &mut outer)?;
        Ok(u32::from_le_bytes(
            outer[NVOS54_STATUS..NVOS54_STATUS + 4].try_into().unwrap(),
        ))
    }

    /// After a forwarded ioctl: a TSG allocation that succeeded gets the
    /// policy's timeslice, and is remembered for the health probe
    /// ([`Self::probe_gpu`]).
    pub(super) fn note_tsg(&mut self, payload: &[u8], resp: &[u8]) {
        if payload.len() < size_of::<IoctlReq>() {
            return;
        }
        let req = read_struct::<IoctlReq>(payload, 0);
        let request = req.cmd as u64;
        if (request >> 8) & 0xFF != b'F' as u64 || request & 0xFF != 0x2b {
            return;
        }
        let head = size_of::<MsgHeader>() + size_of::<IoctlResp>();
        if resp.len() < head || read_struct::<MsgHeader>(resp, 0).status != 0 {
            return;
        }
        let r = read_struct::<IoctlResp>(resp, size_of::<MsgHeader>());
        let data_len = r.data_len as usize;
        let Some(at) = alloc_status_at(data_len) else {
            return;
        };
        let Some(top) = resp.get(head..head + data_len) else {
            return;
        };
        let word = |o: usize| u32::from_le_bytes(top[o..o + 4].try_into().unwrap());
        if word(12) != TSG_CLASS || word(at) != NV_OK {
            return;
        }
        let (client, tsg) = (word(0), word(8));
        let Ok(fd) = self.handles.get_raw(self.current_handle as u64) else {
            return;
        };
        self.remember_tsg(self.current_handle as u64, client, tsg);
        let Some(us) = self.start.sched.timeslice_us else {
            return;
        };
        self.tsg_sched.tsgs += 1;

        let mut p = us.to_le_bytes();
        let res = self.own_rm_control(fd, client, tsg, CMD_SET_TIMESLICE, &mut p);
        if matches!(res, Ok(NV_OK)) {
            self.tsg_sched.timeslice_set += 1;
        } else {
            self.tsg_sched.timeslice_refused += 1;
        }
        if !self.tsg_sched.logged_timeslice {
            self.tsg_sched.logged_timeslice = true;
            let mut got = [0u8; 8];
            let read = self
                .own_rm_control(fd, client, tsg, CMD_GET_TIMESLICE, &mut got)
                .ok()
                .filter(|&s| s == NV_OK)
                .map(|_| u64::from_le_bytes(got));
            match res {
                Ok(NV_OK) => log::info!(
                    "guest TSG 0x{tsg:x}: timeslice {us} us set (RM reads back {read:?} us)"
                ),
                Ok(st) => {
                    log::warn!("guest TSG 0x{tsg:x}: RM refused timeslice {us} us: status 0x{st:x}")
                }
                Err(e) => log::warn!("guest TSG 0x{tsg:x}: SET_TIMESLICE failed: errno {e}"),
            }
        }
    }

    /// A guest RM_CONTROL block, rewritten when it is a SET_TIMESLICE asking
    /// for more than the policy allows. `None`: send it as it came.
    pub(super) fn clamp_guest_timeslice(&mut self, cmd: u32, param_in: &[u8]) -> Option<Vec<u8>> {
        if cmd != CMD_SET_TIMESLICE || self.start.sched.timeslice_us.is_none() {
            return None;
        }
        let at = NVOS54_TOTAL;
        let asked = u64::from_le_bytes(param_in.get(at..at + 8)?.try_into().ok()?);
        let allowed = self.start.sched.clamp_guest_timeslice(asked);
        if allowed == asked {
            return None;
        }
        if self.tsg_sched.guest_clamped == 0 {
            log::info!("guest SET_TIMESLICE {asked} us clamped to {allowed} us (policy)");
        }
        self.tsg_sched.guest_clamped += 1;
        let mut v = param_in.to_vec();
        v[at..at + 8].copy_from_slice(&allowed.to_le_bytes());
        Some(v)
    }

    fn remember_tsg(&mut self, handle: u64, client: u32, tsg: u32) {
        let entry = (handle, client, tsg);
        if self.guest_tsgs.contains(&entry) {
            return;
        }
        if self.guest_tsgs.len() >= MAX_PROBED_TSGS {
            self.guest_tsgs.remove(0);
        }
        self.guest_tsgs.push(entry);
    }

    /// Guest channel groups the health probe would ask about.
    pub fn guest_tsg_count(&self) -> usize {
        self.guest_tsgs.len()
    }

    /// After a host resume: ask RM about every guest channel group with a
    /// harmless control (GET_TIMESLICE). Ones the guest freed answer "not
    /// found" and are forgotten; a GPU that did not come back answers "lost"
    /// on all of them instead of hanging the guest's next wait.
    pub fn probe_gpu(&mut self) -> Health {
        let mut probes = Vec::with_capacity(self.guest_tsgs.len());
        let mut keep = Vec::with_capacity(self.guest_tsgs.len());
        for &(handle, client, tsg) in &self.guest_tsgs {
            let probe = match self.handles.get_raw(handle) {
                Err(_) => Probe::Stale,
                Ok(fd) => {
                    let mut got = [0u8; 8];
                    crate::power::classify(self.own_rm_control(
                        fd,
                        client,
                        tsg,
                        CMD_GET_TIMESLICE,
                        &mut got,
                    ))
                }
            };
            if probe != Probe::Stale {
                keep.push((handle, client, tsg));
            }
            probes.push(probe);
        }
        self.guest_tsgs = keep;
        crate::power::judge(&probes)
    }
}
