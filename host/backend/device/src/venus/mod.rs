//! Venus for Windows guests (docs/VENUS.md): the virtio-gpu commands a
//! `GpuCmd` message carries, checked here and then handed to a
//! [`conduit_venus::Renderer`].
//!
//! The renderer is another process (the NVIDIA Vulkan driver needs more than
//! this sandbox allows), so everything it is told has been checked first:
//! the command's length, that the context and resource it names exist and
//! belong together, that a blob fits region 3 and a mapping lands inside it
//! without overlapping another. The renderer never sees a guest's id that
//! this side has not seen created.
//!
//! `mod.rs` holds the state and the dispatch; display info, the EDID,
//! contexts and capsets are in `cmd.rs` (the EDID's bytes in `edid.rs`), blobs and region 3 in `blob.rs`, fences in `fence.rs` and the
//! scanout in `scanout.rs`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::os::fd::{BorrowedFd, OwnedFd};

use conduit_venus::Renderer;
use protocol::messages::{MsgHeader, MsgType};
use protocol::venus::*;

use crate::display::{DisplayLink, DisplayMode};
use crate::shm::WindowPlacer;

mod blob;
mod cmd;
pub mod edid;
mod fence;
mod scanout;
#[cfg(test)]
mod tests;

pub use fence::Completion;

/// Contexts one VM may hold at once.
pub const MAX_CONTEXTS: usize = 1024;
/// Resources one VM may hold at once.
pub const MAX_RESOURCES: usize = 65536;

const PAGE: u64 = 4096;
/// Rings a fence may name: virglrenderer's proxy refuses `ring_idx` at or
/// above `PROXY_CONTEXT_TIMELINE_COUNT` (64).
pub const MAX_RINGS: u32 = 64;

/// What became of one `GpuCmd`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Answered: this many bytes of response were written.
    Done(usize),
    /// A fenced command: nothing is written yet. Its response comes out of
    /// [`Venus::completions`] under this token once the renderer signals the
    /// fence, and only then may the transport return the chain.
    Held(u64),
}

/// What the transport lends a command: where region 3 is placed, and where
/// frames go. Either may be missing (no frontend channel yet, no display).
#[derive(Clone, Copy, Default)]
pub struct Env<'a> {
    pub window: Option<&'a dyn WindowPlacer>,
    pub display: Option<&'a DisplayLink>,
}

/// A blob resource, as the guest made it.
struct Resource {
    /// The context that created it.
    ctx_id: u32,
    size: u64,
    /// `BLOB_FLAG_*` it was created with.
    flags: u32,
    /// `MAP_CACHE_*` for `RESP_OK_MAP_INFO`.
    map_info: u32,
    /// The renderer's descriptor for its memory, placed in region 3 on map.
    fd: OwnedFd,
    /// Offset in region 3 while mapped.
    mapped: Option<u64>,
    /// Contexts it is attached to.
    attached: HashSet<u32>,
    /// The scanout export, for the size it was made at.
    export: Option<scanout::Export>,
}

/// A virtio-gpu answer, before the response header is put on it.
enum Reply {
    /// `RESP_OK_NODATA`: the one a fenced command waits for its fence with.
    NoData,
    /// A `RESP_OK_*` with a body after the header.
    With(u32, Vec<u8>),
}

/// `Err` is a `RESP_ERR_*`.
type Answer = std::result::Result<Reply, u32>;

pub struct Venus {
    renderer: Box<dyn Renderer>,
    /// Size of region 3.
    hostmem_len: u64,
    /// The configured display, for `GET_DISPLAY_INFO` and `GET_EDID`;
    /// `None` without one.
    display: Option<DisplayMode>,
    contexts: HashSet<u32>,
    resources: HashMap<u32, Resource>,
    /// What is placed in region 3.
    maps: blob::Maps,
    scanout: Option<scanout::Scanout>,
    /// `CONDUIT_VENUS_SCANOUT_MODIFIER`: the modifier every scanout is shown
    /// with, in place of the one its size suggests. For experiments.
    forced_modifier: Option<u64>,
    fences: fence::Fences,
    /// The renderer said it is gone. Everything was released then, and every
    /// command since is answered `RESP_ERR_UNSPEC`.
    lost: bool,
    /// Set with `lost`, until the device has been released for it.
    lose_pending: bool,
    /// Commands served, by name, and refused, by why. Reported at teardown.
    counts: BTreeMap<&'static str, u64>,
}

impl Venus {
    /// A Venus device over `renderer`, with a region 3 of `hostmem_len` bytes
    /// and the display of `display`, if any. A refresh of 0 is taken as
    /// [`edid::DEFAULT_REFRESH_HZ`].
    pub fn new(
        renderer: Box<dyn Renderer>,
        hostmem_len: u64,
        display: Option<DisplayMode>,
    ) -> Self {
        let display = display.map(|d| DisplayMode {
            refresh_hz: match d.refresh_hz {
                0 => edid::DEFAULT_REFRESH_HZ,
                hz => hz,
            },
            ..d
        });
        Self {
            renderer,
            hostmem_len,
            display,
            contexts: HashSet::new(),
            resources: HashMap::new(),
            maps: Default::default(),
            scanout: None,
            forced_modifier: scanout::modifier_override(),
            fences: Default::default(),
            lost: false,
            lose_pending: false,
            counts: BTreeMap::new(),
        }
    }

    pub fn hostmem_len(&self) -> u64 {
        self.hostmem_len
    }

    /// Readable when a fence may have signalled: the transport polls it and
    /// then calls [`Venus::completions`].
    pub fn fence_fd(&self) -> BorrowedFd<'_> {
        self.renderer.fence_fd()
    }

    /// Fenced chains still waiting for the renderer.
    pub fn held(&self) -> usize {
        self.fences.held()
    }

    pub fn contexts(&self) -> usize {
        self.contexts.len()
    }

    pub fn resources(&self) -> usize {
        self.resources.len()
    }

    /// Placements in region 3.
    pub fn mappings(&self) -> usize {
        self.maps.len()
    }

    /// Responses for held chains whose fences have signalled, and for every
    /// held chain once the renderer is lost. Each is a whole response
    /// (`MsgHeader` and virtio-gpu header) for the chain with that token.
    ///
    /// This is where a renderer that dies between commands is found out
    /// (docs/VENUS.md "Reset and close"): it is lost from then on, the
    /// device is released through `env` as on any other loss, and the held
    /// chains come back `RESP_ERR_UNSPEC`.
    pub fn completions(&mut self, env: Env<'_>) -> Vec<Completion> {
        if !self.lost {
            match self.renderer.signalled() {
                Ok(signalled) => {
                    for s in signalled {
                        self.fences.signal(s);
                    }
                }
                Err(e) => {
                    log::warn!("venus: asking for fences: {e}");
                    self.renderer_lost();
                }
            }
            if std::mem::take(&mut self.lose_pending) {
                self.lose(env);
            }
        }
        self.fences.take_ready()
    }

    /// The renderer is gone for good: everything was released, and every
    /// `GpuCmd` is answered `RESP_ERR_UNSPEC` from now on.
    pub fn is_lost(&self) -> bool {
        self.lost
    }

    /// Serve one `GpuCmd`: `payload` is everything after the `MsgHeader`.
    pub fn dispatch(&mut self, payload: &[u8], resp: &mut [u8], env: Env<'_>) -> Outcome {
        if payload.len() + size_of::<MsgHeader>() > GPU_CMD_MAX {
            self.count("refused: too long");
            return Outcome::Done(transport_err(resp, libc::EMSGSIZE));
        }
        let Some(hdr) = CtrlHdr::from_bytes(payload) else {
            self.count("refused: no virtio-gpu header");
            return Outcome::Done(transport_err(resp, libc::EINVAL));
        };
        let answer = if self.lost {
            Err(RESP_ERR_UNSPEC)
        } else if hdr.fenced() && hdr.ring() >= MAX_RINGS {
            // Refused before anything runs: the renderer would refuse the
            // fence only after the command took effect. A fence on a ring
            // below this with no queue bound to it (ring 0 needs none) is
            // worse: vkr fails it and destroys the whole context
            // (vkr_context.c submit_fence, render_context.c dispatch), which
            // the host cannot see from here. Guests bind a queue to a ring
            // before fencing on it.
            Err(RESP_ERR_INVALID_PARAMETER)
        } else {
            self.serve(&hdr, payload, env)
        };
        if let Err(e) = answer {
            self.count(err_name(e));
        }
        // One line per command at debug: what a guest driver asked for and
        // what it got is the first thing bring-up needs.
        log::debug!(
            "venus: command {:#06x} ctx {} ({} bytes) -> {}",
            hdr.ty,
            hdr.ctx_id,
            payload.len(),
            match &answer {
                Ok(_) => "ok",
                Err(e) => err_name(*e),
            }
        );
        // The renderer went away while serving this: release everything
        // now, while the transport is here to take region 3 back.
        if std::mem::take(&mut self.lose_pending) {
            self.lose(env);
        }
        match answer {
            Ok(Reply::NoData) if hdr.fenced() => {
                match self
                    .renderer
                    .create_fence(hdr.ctx_id, hdr.ring(), hdr.fence_id)
                {
                    Ok(()) => Outcome::Held(self.fences.hold(&hdr)),
                    Err(e) => {
                        log::warn!(
                            "venus: fence {} on ctx {} ring {}: {e}",
                            hdr.fence_id,
                            hdr.ctx_id,
                            hdr.ring()
                        );
                        self.renderer_error(&e);
                        if std::mem::take(&mut self.lose_pending) {
                            self.lose(env);
                        }
                        Outcome::Done(reply(resp, &hdr.response(RESP_ERR_UNSPEC), &[]))
                    }
                }
            }
            Ok(Reply::NoData) => Outcome::Done(reply(resp, &hdr.response(RESP_OK_NODATA), &[])),
            Ok(Reply::With(ty, body)) => Outcome::Done(reply(resp, &hdr.response(ty), &body)),
            Err(e) => Outcome::Done(reply(resp, &hdr.response(e), &[])),
        }
    }

    fn serve(&mut self, hdr: &CtrlHdr, b: &[u8], env: Env<'_>) -> Answer {
        // Every command is exactly its struct, or its struct and the
        // trailing bytes it declares; anything else is refused as QEMU does.
        let exact = |n: usize| {
            if b.len() == n {
                Ok(())
            } else {
                Err(RESP_ERR_UNSPEC)
            }
        };
        match hdr.ty {
            CMD_GET_DISPLAY_INFO => {
                exact(GET_DISPLAY_INFO_LEN)?;
                self.count("get_display_info");
                Ok(self.display_info())
            }
            CMD_GET_EDID => {
                exact(GetEdid::LEN)?;
                self.count("get_edid");
                self.edid(&GetEdid::from_bytes(b).expect("length checked"))
            }
            CMD_GET_CAPSET_INFO => {
                exact(GetCapsetInfo::LEN)?;
                self.count("get_capset_info");
                self.capset_info(&GetCapsetInfo::from_bytes(b).expect("length checked"))
            }
            CMD_GET_CAPSET => {
                exact(GetCapset::LEN)?;
                self.count("get_capset");
                self.capset(&GetCapset::from_bytes(b).expect("length checked"))
            }
            CMD_CTX_CREATE => {
                exact(CtxCreate::LEN)?;
                self.count("ctx_create");
                self.ctx_create(&CtxCreate::from_bytes(b).expect("length checked"))
            }
            CMD_CTX_DESTROY => {
                exact(CTX_DESTROY_LEN)?;
                self.count("ctx_destroy");
                self.ctx_destroy(hdr.ctx_id)
            }
            CMD_CTX_ATTACH_RESOURCE => {
                exact(ResourceCmd::LEN)?;
                self.count("ctx_attach_resource");
                let c = ResourceCmd::from_bytes(b).expect("length checked");
                self.ctx_attach(hdr.ctx_id, c.resource_id)
            }
            CMD_CTX_DETACH_RESOURCE => {
                exact(ResourceCmd::LEN)?;
                self.count("ctx_detach_resource");
                let c = ResourceCmd::from_bytes(b).expect("length checked");
                self.ctx_detach(hdr.ctx_id, c.resource_id)
            }
            CMD_SUBMIT_3D => {
                let s = Submit3d::from_bytes(b).ok_or(RESP_ERR_UNSPEC)?;
                exact(Submit3d::LEN + s.size as usize)?;
                self.count("submit_3d");
                self.submit(hdr.ctx_id, &b[Submit3d::LEN..])
            }
            CMD_RESOURCE_CREATE_BLOB => {
                let c = ResourceCreateBlob::from_bytes(b).ok_or(RESP_ERR_UNSPEC)?;
                let entries = (c.nr_entries as usize)
                    .checked_mul(ResourceCreateBlob::MEM_ENTRY_LEN)
                    .ok_or(RESP_ERR_UNSPEC)?;
                exact(ResourceCreateBlob::LEN.saturating_add(entries))?;
                self.count("resource_create_blob");
                self.create_blob(&c)
            }
            CMD_RESOURCE_MAP_BLOB => {
                exact(ResourceMapBlob::LEN)?;
                self.count("resource_map_blob");
                self.map_blob(
                    &ResourceMapBlob::from_bytes(b).expect("length checked"),
                    env,
                )
            }
            CMD_RESOURCE_UNMAP_BLOB => {
                exact(ResourceCmd::LEN)?;
                self.count("resource_unmap_blob");
                let c = ResourceCmd::from_bytes(b).expect("length checked");
                self.unmap_blob(c.resource_id, env)
            }
            CMD_RESOURCE_UNREF => {
                exact(ResourceCmd::LEN)?;
                self.count("resource_unref");
                let c = ResourceCmd::from_bytes(b).expect("length checked");
                self.unref(c.resource_id, env)
            }
            CMD_SET_SCANOUT_BLOB => {
                exact(SetScanoutBlob::LEN)?;
                self.count("set_scanout_blob");
                self.set_scanout_blob(&SetScanoutBlob::from_bytes(b).expect("length checked"), env)
            }
            CMD_RESOURCE_FLUSH => {
                exact(ResourceFlush::LEN)?;
                self.count("resource_flush");
                self.flush(&ResourceFlush::from_bytes(b).expect("length checked"), env)
            }
            other => {
                log::debug!("venus: command {other:#06x} is not served");
                Err(RESP_ERR_UNSPEC)
            }
        }
    }

    /// A renderer error as the guest hears it. A renderer that is gone is
    /// noted, and the device released after the command (see `dispatch`).
    fn renderer_error(&mut self, e: &conduit_venus::Error) -> u32 {
        use conduit_venus::Error as E;
        match e {
            E::Disconnected => {
                self.renderer_lost();
                RESP_ERR_UNSPEC
            }
            E::NoContext(_) => RESP_ERR_INVALID_CONTEXT_ID,
            E::NoResource(_) => RESP_ERR_INVALID_RESOURCE_ID,
            E::Refused(_) | E::Io(_) => RESP_ERR_UNSPEC,
        }
    }

    /// Note the renderer as gone; the device is released after the command
    /// (or in `completions`), while the transport is there to take region 3
    /// back.
    fn renderer_lost(&mut self) {
        if !self.lost {
            log::error!("venus: the renderer is gone; releasing every context and resource");
        }
        self.lose_pending |= !self.lost;
        self.lost = true;
    }

    /// The renderer died: everything on this side goes, region 3 is emptied,
    /// and the held chains come back from [`Venus::completions`] as
    /// `RESP_ERR_UNSPEC`.
    fn lose(&mut self, env: Env<'_>) {
        self.release(env.window, env.display, false);
        self.fences.fail_all();
    }

    /// Drop every context, resource, mapping and the scanout. `tell` is
    /// whether the renderer is still there to be told.
    fn release(
        &mut self,
        window: Option<&dyn WindowPlacer>,
        display: Option<&DisplayLink>,
        tell: bool,
    ) {
        for (offset, len, id) in self.maps.drain() {
            if let Some(w) = window
                && let Err(e) = w.withdraw_blob(offset, len)
            {
                log::warn!("venus: withdrawing resource {id} from region 3: {e}");
            }
        }
        if self.scanout.take().is_some()
            && let Some(link) = display
        {
            link.disable();
        }
        if tell {
            for (&id, r) in &self.resources {
                for &ctx in &r.attached {
                    self.renderer.ctx_detach(ctx, id);
                }
            }
            for &id in self.resources.keys() {
                self.renderer.unref(id);
            }
            for &ctx in &self.contexts {
                self.renderer.ctx_destroy(ctx);
            }
        }
        self.resources.clear();
        self.contexts.clear();
    }

    /// Device reset or backend exit (docs/VENUS.md "Reset and close"):
    /// every context and resource is destroyed, region 3 emptied, and the
    /// held chains returned as `RESP_ERR_UNSPEC` -- the completions are
    /// returned for a transport whose queue is still there to take them.
    ///
    /// `window` is `None` on a device reset: the frontend has dropped every
    /// placement already, as for the window and the aperture.
    pub fn reset(&mut self, window: Option<&dyn WindowPlacer>) -> Vec<Completion> {
        let (c, r, m, h) = (
            self.contexts.len(),
            self.resources.len(),
            self.maps.len(),
            self.fences.held(),
        );
        if c + r + m + h > 0 {
            log::info!(
                "venus: releasing {c} context(s), {r} resource(s), {m} region 3 mapping(s) and {h} held chain(s)"
            );
        }
        let tell = !self.lost;
        self.release(window, None, tell);
        self.fences.fail_all();
        // A renderer that was lost stays lost: a new boot has nothing to
        // talk to either.
        self.fences.take_ready()
    }

    /// Log what was served; called at backend teardown.
    pub fn report(&self) {
        let total: u64 = self.counts.values().sum();
        log::info!(
            "venus: {total} command(s): {}",
            self.counts
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
    }

    fn count(&mut self, what: &'static str) {
        *self.counts.entry(what).or_insert(0) += 1;
    }
}

fn err_name(e: u32) -> &'static str {
    match e {
        RESP_ERR_UNSPEC => "err_unspec",
        RESP_ERR_OUT_OF_MEMORY => "err_out_of_memory",
        RESP_ERR_INVALID_SCANOUT_ID => "err_invalid_scanout_id",
        RESP_ERR_INVALID_RESOURCE_ID => "err_invalid_resource_id",
        RESP_ERR_INVALID_CONTEXT_ID => "err_invalid_context_id",
        RESP_ERR_INVALID_PARAMETER => "err_invalid_parameter",
        _ => "err_other",
    }
}

/// Write `MsgHeader` | `hdr` | `body`. A response buffer too small for it is
/// a transport error, not a virtio-gpu one: the guest cannot read the answer.
fn reply(resp: &mut [u8], hdr: &CtrlHdr, body: &[u8]) -> usize {
    let at = size_of::<MsgHeader>();
    let need = at + CTRL_HDR_LEN + body.len();
    if resp.len() < need {
        return transport_err(resp, libc::ENOSPC);
    }
    write_msg_header(resp, &MsgHeader::ok(MsgType::GpuCmd, 0));
    resp[at..at + CTRL_HDR_LEN].copy_from_slice(&hdr.to_bytes());
    resp[at + CTRL_HDR_LEN..need].copy_from_slice(body);
    need
}

/// A bare `MsgHeader` with a negative errno: the message itself was bad.
fn transport_err(resp: &mut [u8], errno: i32) -> usize {
    if resp.len() < size_of::<MsgHeader>() {
        return 0;
    }
    write_msg_header(resp, &MsgHeader::err(MsgType::GpuCmd, errno))
}

fn write_msg_header(resp: &mut [u8], h: &MsgHeader) -> usize {
    for (i, v) in [h.msg_type, h.handle, h.status as u32, h.padding]
        .iter()
        .enumerate()
    {
        resp[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }
    size_of::<MsgHeader>()
}
