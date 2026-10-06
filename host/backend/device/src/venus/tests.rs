//! Venus against `conduit_venus::mock::Mock`: the checks, the fences, region
//! 3 and the scanout, without a GPU.

use super::*;
use crate::display::wire;
use conduit_venus::mock::Mock;
use conduit_venus::{Blob, CapsetInfo, Dmabuf, ScanoutLayout, Signalled};
use std::collections::HashMap;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, RawFd};
use std::sync::{Arc, Mutex};

/// The mock behind a handle the test keeps, so what reached the renderer can
/// be looked at. `gone` makes every call fail as a dead renderer's would.
#[derive(Clone)]
struct Shared {
    mock: Arc<Mutex<Mock>>,
    fence: Arc<OwnedFd>,
    gone: Arc<std::sync::atomic::AtomicBool>,
    /// While set, signalled fences are kept back here instead of returned.
    holdback: Arc<Mutex<Option<Vec<Signalled>>>>,
    /// Kept back, now let go: returned by the next `signalled`.
    released: Arc<Mutex<Vec<Signalled>>>,
    /// Every `export_scanout` asked for: resource and layout.
    exports: Arc<Mutex<Vec<(u32, ScanoutLayout)>>>,
}

impl Shared {
    fn new() -> Self {
        let mock = Mock::new();
        let fence = mock.fence_fd().try_clone_to_owned().unwrap();
        Self {
            mock: Arc::new(Mutex::new(mock)),
            fence: Arc::new(fence),
            gone: Default::default(),
            holdback: Default::default(),
            released: Default::default(),
            exports: Default::default(),
        }
    }

    fn check(&self) -> conduit_venus::Result<()> {
        if self.gone.load(std::sync::atomic::Ordering::Relaxed) {
            Err(conduit_venus::Error::Disconnected)
        } else {
            Ok(())
        }
    }

    /// Keep signalled fences back until [`Shared::release`]: the GPU is busy.
    fn hold_fences(&self) {
        self.holdback.lock().unwrap().get_or_insert_with(Vec::new);
    }

    /// The GPU caught up: everything kept back is signalled at the next ask.
    fn release(&self) {
        let kept = self.holdback.lock().unwrap().take().unwrap_or_default();
        self.released.lock().unwrap().extend(kept);
    }
}

impl Renderer for Shared {
    fn capset_info(&mut self, index: u32) -> conduit_venus::Result<CapsetInfo> {
        self.check()?;
        self.mock.lock().unwrap().capset_info(index)
    }
    fn capset(&mut self, id: u32, version: u32) -> conduit_venus::Result<Vec<u8>> {
        self.check()?;
        self.mock.lock().unwrap().capset(id, version)
    }
    fn ctx_create(
        &mut self,
        ctx_id: u32,
        capset_id: u32,
        name: &[u8],
    ) -> conduit_venus::Result<()> {
        self.check()?;
        self.mock
            .lock()
            .unwrap()
            .ctx_create(ctx_id, capset_id, name)
    }
    fn ctx_destroy(&mut self, ctx_id: u32) {
        self.mock.lock().unwrap().ctx_destroy(ctx_id)
    }
    fn ctx_attach(&mut self, ctx_id: u32, res_id: u32) -> conduit_venus::Result<()> {
        self.check()?;
        self.mock.lock().unwrap().ctx_attach(ctx_id, res_id)
    }
    fn ctx_detach(&mut self, ctx_id: u32, res_id: u32) {
        self.mock.lock().unwrap().ctx_detach(ctx_id, res_id)
    }
    fn submit(&mut self, ctx_id: u32, commands: &[u8]) -> conduit_venus::Result<()> {
        self.check()?;
        self.mock.lock().unwrap().submit(ctx_id, commands)
    }
    fn create_blob(
        &mut self,
        ctx_id: u32,
        res_id: u32,
        blob_id: u64,
        size: u64,
        flags: u32,
    ) -> conduit_venus::Result<Blob> {
        self.check()?;
        self.mock
            .lock()
            .unwrap()
            .create_blob(ctx_id, res_id, blob_id, size, flags)
    }
    fn unref(&mut self, res_id: u32) {
        self.mock.lock().unwrap().unref(res_id)
    }
    fn create_fence(
        &mut self,
        ctx_id: u32,
        ring_idx: u32,
        fence_id: u64,
    ) -> conduit_venus::Result<()> {
        self.check()?;
        self.mock
            .lock()
            .unwrap()
            .create_fence(ctx_id, ring_idx, fence_id)
    }
    fn fence_fd(&self) -> BorrowedFd<'_> {
        self.fence.as_fd()
    }
    fn signalled(&mut self) -> conduit_venus::Result<Vec<Signalled>> {
        self.check()?;
        let mut now = self.mock.lock().unwrap().signalled()?;
        Ok(match self.holdback.lock().unwrap().as_mut() {
            Some(kept) => {
                kept.append(&mut now);
                Vec::new()
            }
            None => {
                now.splice(0..0, std::mem::take(&mut *self.released.lock().unwrap()));
                now
            }
        })
    }
    fn export_scanout(
        &mut self,
        res_id: u32,
        layout: ScanoutLayout,
    ) -> conduit_venus::Result<Dmabuf> {
        self.check()?;
        self.exports.lock().unwrap().push((res_id, layout));
        self.mock.lock().unwrap().export_scanout(res_id, layout)
    }
    fn features(&mut self) -> u32 {
        self.mock.lock().unwrap().features()
    }
    fn import_dmabuf(
        &mut self,
        res_id: u32,
        fd: BorrowedFd<'_>,
        size: u64,
    ) -> conduit_venus::Result<()> {
        self.check()?;
        self.mock.lock().unwrap().import_dmabuf(res_id, fd, size)
    }
    fn import_guest_pages(
        &mut self,
        res_id: u32,
        ram: BorrowedFd<'_>,
        runs: &[conduit_venus::PageRun],
    ) -> conduit_venus::Result<()> {
        self.check()?;
        self.mock
            .lock()
            .unwrap()
            .import_guest_pages(res_id, ram, runs)
    }
}

/// The RM side of the backend: objects by (file, GEM handle), each a memfd
/// of its size exported fresh per ask, with the modifier its import had.
#[derive(Default)]
struct FakeRm {
    objects: HashMap<(u32, u32), (u64, Option<u64>)>,
    /// The `attr` RM allocated each with, when the test says.
    placements: HashMap<(u32, u32), u32>,
    /// The file descriptors handed out, by inode, so a test can tell the
    /// renderer got the same object.
    exported: Mutex<Vec<u64>>,
}

impl RmExports for FakeRm {
    fn export(&self, rm: u32, gem: u32) -> std::result::Result<crate::nvidia::RmObject, i32> {
        let &(size, modifier) =
            self.objects
                .get(&(rm, gem))
                .ok_or(if rm == 7 { libc::ENOENT } else { libc::EBADF })?;
        let dmabuf = conduit_venus::mock::memfd(size).map_err(|_| libc::EIO)?;
        self.exported.lock().unwrap().push(inode(dmabuf.as_fd()));
        Ok(crate::nvidia::RmObject {
            dmabuf,
            modifier,
            placement: self
                .placements
                .get(&(rm, gem))
                .map(|&attr| crate::nvidia::RmPlacement { attr }),
        })
    }
}

fn inode(fd: BorrowedFd<'_>) -> u64 {
    // SAFETY: fstat into a zeroed local.
    unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        assert_eq!(libc::fstat(fd.as_raw_fd(), &mut st), 0);
        st.st_ino
    }
}

/// Region 3 as a plain mapping in this process, so a placed blob can be
/// written through it; records every call.
struct Region {
    base: usize,
    len: u64,
    calls: Mutex<Vec<(&'static str, u64, u64)>>,
}

impl Region {
    fn new(len: u64) -> Arc<Self> {
        // SAFETY: a fresh anonymous reservation, unmapped in Drop.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len as usize,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        assert_ne!(base, libc::MAP_FAILED);
        Arc::new(Self {
            base: base as usize,
            len,
            calls: Mutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> Vec<(&'static str, u64, u64)> {
        self.calls.lock().unwrap().clone()
    }
}

impl Drop for Region {
    fn drop(&mut self) {
        // SAFETY: the reservation made in `new`.
        unsafe { libc::munmap(self.base as *mut libc::c_void, self.len as usize) };
    }
}

impl WindowPlacer for Region {
    fn place(&self, _: u64, _: u64, _: RawFd, _: u64, _: bool) -> crate::error::Result<()> {
        unreachable!("Venus places in region 3 only")
    }
    fn withdraw(&self, _: u64, _: u64) -> crate::error::Result<()> {
        unreachable!("Venus places in region 3 only")
    }
    fn place_blob(&self, offset: u64, len: u64, fd: RawFd) -> crate::error::Result<()> {
        assert!(offset + len <= self.len);
        // SAFETY: inside our own reservation.
        let p = unsafe {
            libc::mmap(
                (self.base + offset as usize) as *mut libc::c_void,
                len as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_FIXED,
                fd,
                0,
            )
        };
        assert_ne!(p, libc::MAP_FAILED);
        self.calls.lock().unwrap().push(("place", offset, len));
        Ok(())
    }
    fn withdraw_blob(&self, offset: u64, len: u64) -> crate::error::Result<()> {
        // SAFETY: back to an inaccessible reservation, as a hole must not be.
        unsafe {
            libc::mmap(
                (self.base + offset as usize) as *mut libc::c_void,
                len as usize,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_FIXED | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        self.calls.lock().unwrap().push(("withdraw", offset, len));
        Ok(())
    }
}

const HOSTMEM: u64 = 64 << 20;

fn hdr(ty: u32, ctx_id: u32) -> CtrlHdr {
    CtrlHdr {
        ty,
        ctx_id,
        ..Default::default()
    }
}

fn fenced(ty: u32, ctx_id: u32, fence_id: u64) -> CtrlHdr {
    CtrlHdr {
        ty,
        flags: FLAG_FENCE,
        fence_id,
        ctx_id,
        ..Default::default()
    }
}

struct Rig {
    venus: Venus,
    r: Shared,
    region: Arc<Region>,
    resp: Vec<u8>,
    rm: FakeRm,
    /// Guest RAM, for guest-memory blobs; none unless a test gives it.
    ram: Option<crate::guestmem::fake::FakeRam>,
}

impl Rig {
    fn new() -> Self {
        let r = Shared::new();
        Self {
            venus: Venus::new(
                Box::new(r.clone()),
                HOSTMEM,
                Some(DisplayMode {
                    width: 1280,
                    height: 720,
                    refresh_hz: 60,
                }),
            ),
            r,
            region: Region::new(HOSTMEM),
            resp: vec![0u8; 64 * 1024],
            rm: FakeRm::default(),
            ram: None,
        }
    }

    fn send_with(&mut self, cmd: &[u8], display: Option<&DisplayLink>) -> Outcome {
        let env = Env {
            window: Some(&*self.region),
            display,
            rm: Some(&self.rm),
            ram: self
                .ram
                .as_ref()
                .map(|r| r as &dyn crate::guestmem::GuestRam),
        };
        self.venus.dispatch(cmd, &mut self.resp, env)
    }

    /// Send, expecting an answer now; its virtio-gpu header and body.
    fn send(&mut self, cmd: &[u8]) -> (CtrlHdr, Vec<u8>) {
        self.send_on(cmd, None)
    }

    fn send_on(&mut self, cmd: &[u8], display: Option<&DisplayLink>) -> (CtrlHdr, Vec<u8>) {
        let Outcome::Done(n) = self.send_with(cmd, display) else {
            panic!("held");
        };
        assert_eq!(status(&self.resp), 0, "transport status");
        assert_eq!(
            u32::from_le_bytes(self.resp[0..4].try_into().unwrap()),
            MsgType::GpuCmd as u32
        );
        let h = CtrlHdr::from_bytes(&self.resp[16..n]).unwrap();
        (h, self.resp[16 + CTRL_HDR_LEN..n].to_vec())
    }

    /// Completions, with region 3 there to be emptied if the renderer is
    /// found dead.
    fn completions(&mut self) -> Vec<Completion> {
        let env = Env {
            window: Some(&*self.region),
            display: None,
            rm: None,
            ram: None,
        };
        self.venus.completions(env)
    }

    fn ty(&mut self, cmd: &[u8]) -> u32 {
        self.send(cmd).0.ty
    }

    fn ctx(&mut self, id: u32) {
        let c = CtxCreate {
            hdr: hdr(CMD_CTX_CREATE, id),
            nlen: 4,
            context_init: CAPSET_VENUS,
            debug_name: {
                let mut n = [0u8; 64];
                n[..4].copy_from_slice(b"test");
                n
            },
        };
        assert_eq!(self.ty(&c.to_bytes()), RESP_OK_NODATA);
    }

    fn blob(&mut self, ctx: u32, id: u32, size: u64) -> u32 {
        self.ty(&blob_cmd(ctx, id, size))
    }

    fn map(&mut self, id: u32, offset: u64) -> u32 {
        self.ty(&map_cmd(id, offset))
    }
}

fn blob_cmd(ctx: u32, id: u32, size: u64) -> [u8; ResourceCreateBlob::LEN] {
    ResourceCreateBlob {
        hdr: hdr(CMD_RESOURCE_CREATE_BLOB, ctx),
        resource_id: id,
        blob_mem: BLOB_MEM_HOST3D,
        blob_flags: BLOB_FLAG_USE_MAPPABLE,
        nr_entries: 0,
        blob_id: id as u64,
        size,
    }
    .to_bytes()
}

fn map_cmd(id: u32, offset: u64) -> [u8; ResourceMapBlob::LEN] {
    ResourceMapBlob {
        hdr: hdr(CMD_RESOURCE_MAP_BLOB, 0),
        resource_id: id,
        padding: 0,
        offset,
    }
    .to_bytes()
}

fn res_cmd(ty: u32, ctx: u32, id: u32) -> [u8; ResourceCmd::LEN] {
    ResourceCmd {
        hdr: hdr(ty, ctx),
        resource_id: id,
        padding: 0,
    }
    .to_bytes()
}

fn submit_cmd(h: CtrlHdr, stream: &[u8]) -> Vec<u8> {
    let mut v = Submit3d {
        hdr: h,
        size: stream.len() as u32,
        padding: 0,
    }
    .to_bytes()
    .to_vec();
    v.extend_from_slice(stream);
    v
}

fn scanout_cmd(id: u32, w: u32, h: u32) -> [u8; SetScanoutBlob::LEN] {
    SetScanoutBlob {
        hdr: hdr(CMD_SET_SCANOUT_BLOB, 0),
        r: Rect {
            x: 0,
            y: 0,
            width: w,
            height: h,
        },
        scanout_id: 0,
        resource_id: id,
        width: w,
        height: h,
        format: format::B8G8R8X8_UNORM,
        padding: 0,
        strides: [w * 4, 0, 0, 0],
        offsets: [0; 4],
    }
    .to_bytes()
}

fn flush_cmd(id: u32) -> [u8; ResourceFlush::LEN] {
    ResourceFlush {
        hdr: hdr(CMD_RESOURCE_FLUSH, 0),
        r: Rect::default(),
        resource_id: id,
        padding: 0,
    }
    .to_bytes()
}

fn status(resp: &[u8]) -> i32 {
    i32::from_le_bytes(resp[8..12].try_into().unwrap())
}

/// A display link with one client that wants frames, and the client's end.
fn display() -> (Arc<DisplayLink>, OwnedFd) {
    let mut sv = [0i32; 2];
    // SAFETY: a fresh pair into a local array.
    assert_eq!(
        unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, sv.as_mut_ptr()) },
        0
    );
    let link = DisplayLink::new(None);
    // SAFETY: both are fresh descriptors owned by nobody else.
    link.adopt(unsafe { OwnedFd::from_raw_fd(sv[0]) });
    (link, unsafe { OwnedFd::from_raw_fd(sv[1]) })
}

/// The whole life of a Venus guest's frame, in the order Mesa and the KMD
/// send it.
#[test]
fn a_guest_renders_a_frame_from_capset_to_reset() {
    let mut t = Rig::new();
    let (link, broker) = display();

    // Capset info and the capset.
    let (h, body) = t.send(
        &GetCapsetInfo {
            hdr: hdr(CMD_GET_CAPSET_INFO, 0),
            capset_index: 0,
            padding: 0,
        }
        .to_bytes(),
    );
    assert_eq!(h.ty, RESP_OK_CAPSET_INFO);
    let mut full = h.to_bytes().to_vec();
    full.extend_from_slice(&body);
    let info = RespCapsetInfo::from_bytes(&full).unwrap();
    assert_eq!((info.capset_id, info.capset_max_size), (CAPSET_VENUS, 160));
    let (h, body) = t.send(
        &GetCapset {
            hdr: hdr(CMD_GET_CAPSET, 0),
            capset_id: CAPSET_VENUS,
            capset_version: 0,
        }
        .to_bytes(),
    );
    assert_eq!((h.ty, body.len()), (RESP_OK_CAPSET, 160));

    // Display info: the configured display, one scanout.
    let (h, body) = t.send(&hdr(CMD_GET_DISPLAY_INFO, 0).to_bytes());
    assert_eq!(h.ty, RESP_OK_DISPLAY_INFO);
    let mut full = h.to_bytes().to_vec();
    full.extend_from_slice(&body);
    let di = RespDisplayInfo::from_bytes(&full).unwrap();
    assert_eq!(
        (
            di.pmodes[0].enabled,
            di.pmodes[0].r.width,
            di.pmodes[0].r.height
        ),
        (1, 1280, 720)
    );
    assert!(di.pmodes[1..].iter().all(|p| p.enabled == 0));

    // A context, a blob, mapped at the guest's offset.
    t.ctx(1);
    assert!(t.r.mock.lock().unwrap().contexts.contains(&1));
    assert_eq!(t.blob(1, 10, 1 << 20), RESP_OK_NODATA);
    let (h, body) = t.send(&map_cmd(10, 2 << 20));
    assert_eq!(h.ty, RESP_OK_MAP_INFO);
    assert_eq!(
        u32::from_le_bytes(body[0..4].try_into().unwrap()),
        MAP_CACHE_CACHED
    );
    assert_eq!(t.region.calls(), vec![("place", 2 << 20, 1 << 20)]);
    // The guest's view and the renderer's are the same memory.
    let p = (t.region.base + (2 << 20)) as *mut u8;
    // SAFETY: inside the placement just made.
    unsafe { p.write(0x5a) };
    assert_eq!(t.venus.mappings(), 1);

    // A command stream, unfenced: answered now.
    assert_eq!(
        t.ty(&submit_cmd(hdr(CMD_SUBMIT_3D, 1), b"vkCmd...")),
        RESP_OK_NODATA
    );
    assert_eq!(
        t.r.mock.lock().unwrap().submitted,
        vec![(1, b"vkCmd...".to_vec())]
    );

    // Fenced: held until the renderer signals.
    t.r.hold_fences();
    let Outcome::Held(token) = t.send_with(&submit_cmd(fenced(CMD_SUBMIT_3D, 1, 7), b"x"), None)
    else {
        panic!("a fenced submit must be held");
    };
    assert_eq!(t.venus.held(), 1);
    assert!(t.completions().is_empty(), "not signalled yet");
    t.r.release();
    let done = t.completions();
    assert_eq!(done.len(), 1);
    assert_eq!(done[0].token, token);
    assert_eq!(status(&done[0].resp), 0);
    let h = CtrlHdr::from_bytes(&done[0].resp[16..]).unwrap();
    assert_eq!(
        (h.ty, h.flags, h.fence_id, h.ctx_id),
        (RESP_OK_NODATA, FLAG_FENCE, 7, 1)
    );
    assert_eq!(t.venus.held(), 0);

    // Scanout and a flush: the renderer exports once, the frame reaches the
    // display client.
    assert_eq!(t.blob(1, 11, 1280 * 720 * 4), RESP_OK_NODATA);
    assert_eq!(t.ty(&scanout_cmd(11, 1280, 720)), RESP_OK_NODATA);
    assert_eq!(
        t.venus.scanout_state(),
        Some((11, 1280, 720, format::B8G8R8X8_UNORM, 5120, 0))
    );
    for _ in 0..3 {
        assert_eq!(t.send_on(&flush_cmd(11), Some(&link)).0.ty, RESP_OK_NODATA);
    }
    use std::sync::atomic::Ordering::Relaxed;
    assert_eq!(link.stats.sent.load(Relaxed), 3);
    // A flush of a resource that is not the scanout shows nothing.
    assert_eq!(t.send_on(&flush_cmd(10), Some(&link)).0.ty, RESP_OK_NODATA);
    assert_eq!(link.stats.sent.load(Relaxed), 3);

    // Unref: the mapped blob is withdrawn first, the scanout's goes off.
    assert_eq!(t.ty(&res_cmd(CMD_RESOURCE_UNREF, 0, 10)), RESP_OK_NODATA);
    assert_eq!(t.region.calls()[1], ("withdraw", 2 << 20, 1 << 20));
    assert!(!t.r.mock.lock().unwrap().resources.contains_key(&10));
    assert_eq!(t.venus.mappings(), 0);
    assert_eq!(
        t.send_on(&res_cmd(CMD_RESOURCE_UNREF, 0, 11), Some(&link))
            .0
            .ty,
        RESP_OK_NODATA
    );
    assert_eq!(t.venus.scanout_state(), None);

    // Reset: everything goes, on both sides.
    assert_eq!(t.blob(1, 12, 4096), RESP_OK_NODATA);
    assert_eq!(t.map(12, 0), RESP_OK_MAP_INFO);
    t.r.hold_fences();
    let Outcome::Held(token) = t.send_with(&submit_cmd(fenced(CMD_SUBMIT_3D, 1, 8), b"y"), None)
    else {
        panic!("held");
    };
    let region = t.region.clone();
    let done = t.venus.reset(Some(&*region), None);
    assert_eq!(done.len(), 1);
    assert_eq!(done[0].token, token);
    assert_eq!(
        CtrlHdr::from_bytes(&done[0].resp[16..]).unwrap().ty,
        RESP_ERR_UNSPEC
    );
    assert_eq!(region.calls().last(), Some(&("withdraw", 0, 4096)));
    assert_eq!(
        (t.venus.contexts(), t.venus.resources(), t.venus.mappings()),
        (0, 0, 0)
    );
    let m = t.r.mock.lock().unwrap();
    assert!(m.contexts.is_empty() && m.resources.is_empty());
    drop(m);
    // And the device works again, from nothing.
    t.ctx(1);
    drop(broker);
}

/// The checks of docs/VENUS.md, each before the renderer sees anything.
#[test]
fn bad_ids_are_refused_before_the_renderer() {
    let mut t = Rig::new();
    // No such context.
    assert_eq!(t.blob(5, 10, 4096), RESP_ERR_INVALID_CONTEXT_ID);
    assert_eq!(
        t.ty(&submit_cmd(hdr(CMD_SUBMIT_3D, 5), b"")),
        RESP_ERR_INVALID_CONTEXT_ID
    );
    assert_eq!(
        t.ty(&hdr(CMD_CTX_DESTROY, 5).to_bytes()),
        RESP_ERR_INVALID_CONTEXT_ID
    );
    t.ctx(1);
    // A context id twice, or zero.
    let again = CtxCreate {
        hdr: hdr(CMD_CTX_CREATE, 1),
        context_init: CAPSET_VENUS,
        ..Default::default()
    };
    assert_eq!(t.ty(&again.to_bytes()), RESP_ERR_INVALID_CONTEXT_ID);
    let zero = CtxCreate {
        hdr: hdr(CMD_CTX_CREATE, 0),
        ..again
    };
    assert_eq!(t.ty(&zero.to_bytes()), RESP_ERR_INVALID_CONTEXT_ID);
    // Any capset but Venus.
    let virgl = CtxCreate {
        hdr: hdr(CMD_CTX_CREATE, 2),
        context_init: 1,
        ..again
    };
    assert_eq!(t.ty(&virgl.to_bytes()), RESP_ERR_INVALID_PARAMETER);

    // No such resource.
    assert_eq!(t.map(10, 0), RESP_ERR_INVALID_RESOURCE_ID);
    assert_eq!(
        t.ty(&res_cmd(CMD_RESOURCE_UNREF, 0, 10)),
        RESP_ERR_INVALID_RESOURCE_ID
    );
    assert_eq!(
        t.ty(&res_cmd(CMD_CTX_ATTACH_RESOURCE, 1, 10)),
        RESP_ERR_INVALID_RESOURCE_ID
    );
    assert_eq!(t.ty(&flush_cmd(10)), RESP_ERR_INVALID_RESOURCE_ID);
    assert_eq!(t.ty(&scanout_cmd(10, 64, 64)), RESP_ERR_INVALID_RESOURCE_ID);
    // Resource ids are unique, and zero is none.
    assert_eq!(t.blob(1, 10, 4096), RESP_OK_NODATA);
    assert_eq!(t.blob(1, 10, 4096), RESP_ERR_INVALID_RESOURCE_ID);
    assert_eq!(t.blob(1, 0, 4096), RESP_ERR_INVALID_RESOURCE_ID);
    // Belonging together: detaching from a context it is not attached to.
    t.ctx(2);
    assert_eq!(
        t.ty(&res_cmd(CMD_CTX_DETACH_RESOURCE, 2, 10)),
        RESP_ERR_INVALID_RESOURCE_ID
    );
    assert_eq!(
        t.ty(&res_cmd(CMD_CTX_ATTACH_RESOURCE, 2, 10)),
        RESP_OK_NODATA
    );
    assert_eq!(
        t.ty(&res_cmd(CMD_CTX_DETACH_RESOURCE, 2, 10)),
        RESP_OK_NODATA
    );
    assert_eq!(
        t.ty(&res_cmd(CMD_CTX_DETACH_RESOURCE, 2, 10)),
        RESP_ERR_INVALID_RESOURCE_ID
    );
    // Destroying a context detaches what it had; the resource stays.
    assert_eq!(t.ty(&hdr(CMD_CTX_DESTROY, 1).to_bytes()), RESP_OK_NODATA);
    assert_eq!(t.venus.resources(), 1);
    assert_eq!(
        t.ty(&res_cmd(CMD_CTX_DETACH_RESOURCE, 1, 10)),
        RESP_ERR_INVALID_CONTEXT_ID
    );
    // Scanout 1 does not exist.
    let mut s = SetScanoutBlob::from_bytes(&scanout_cmd(10, 32, 32)).unwrap();
    s.scanout_id = 1;
    assert_eq!(t.ty(&s.to_bytes()), RESP_ERR_INVALID_SCANOUT_ID);
    // Capset index 1 does not either.
    let ci = GetCapsetInfo {
        hdr: hdr(CMD_GET_CAPSET_INFO, 0),
        capset_index: 1,
        padding: 0,
    };
    assert_eq!(t.ty(&ci.to_bytes()), RESP_ERR_INVALID_PARAMETER);
    assert!(t.r.mock.lock().unwrap().submitted.is_empty());
}

/// A blob need not be a page multiple (Mesa's Venus ring is 128 KiB + 196,
/// as the Windows guest asks): it is served and maps as whole pages.
#[test]
fn blobs_of_any_size_map_as_whole_pages() {
    let mut t = Rig::new();
    t.ctx(1);
    assert_eq!(t.blob(1, 13, 131_268), RESP_OK_NODATA);
    // 33 pages: one page short of room at the end of region 3 is refused...
    assert_eq!(t.map(13, HOSTMEM - 131_072), RESP_ERR_INVALID_PARAMETER);
    // ...and exactly 33 pages of room fits.
    assert_eq!(t.map(13, HOSTMEM - 135_168), RESP_OK_MAP_INFO);
}

/// Blob sizes and region 3 placements.
#[test]
fn region_3_placements_are_checked() {
    let mut t = Rig::new();
    t.ctx(1);
    // Size: nonzero, inside region 3 once rounded up to a page.
    assert_eq!(t.blob(1, 10, 0), RESP_ERR_INVALID_PARAMETER);
    assert_eq!(t.blob(1, 10, HOSTMEM + 4096), RESP_ERR_INVALID_PARAMETER);
    assert_eq!(t.blob(1, 10, HOSTMEM + 1), RESP_ERR_INVALID_PARAMETER);
    // Guest-memory blobs are not served.
    let mut g = ResourceCreateBlob::from_bytes(&blob_cmd(1, 10, 4096)).unwrap();
    g.blob_mem = BLOB_MEM_GUEST;
    assert_eq!(t.ty(&g.to_bytes()), RESP_ERR_INVALID_PARAMETER);
    assert!(t.r.mock.lock().unwrap().resources.is_empty());

    assert_eq!(t.blob(1, 10, 8192), RESP_OK_NODATA);
    assert_eq!(t.blob(1, 11, 8192), RESP_OK_NODATA);
    assert_eq!(t.blob(1, 12, HOSTMEM), RESP_OK_NODATA);
    // Misaligned.
    assert_eq!(t.map(10, 100), RESP_ERR_INVALID_PARAMETER);
    // Past the end, and wrapping.
    assert_eq!(t.map(10, HOSTMEM - 4096), RESP_ERR_INVALID_PARAMETER);
    assert_eq!(t.map(10, u64::MAX & !4095), RESP_ERR_INVALID_PARAMETER);
    assert_eq!(t.map(10, 16384), RESP_OK_MAP_INFO);
    // Twice.
    assert_eq!(t.map(10, 65536), RESP_ERR_INVALID_PARAMETER);
    // Overlapping it from below, from above, and exactly.
    assert_eq!(t.map(11, 12288), RESP_ERR_INVALID_PARAMETER);
    assert_eq!(t.map(11, 20480), RESP_ERR_INVALID_PARAMETER);
    assert_eq!(t.map(11, 16384), RESP_ERR_INVALID_PARAMETER);
    // A whole-region blob overlaps whatever is there.
    assert_eq!(t.map(12, 0), RESP_ERR_INVALID_PARAMETER);
    // Adjacent on either side is fine.
    assert_eq!(t.map(11, 8192), RESP_OK_MAP_INFO);
    assert_eq!(
        t.ty(&res_cmd(CMD_RESOURCE_UNMAP_BLOB, 0, 11)),
        RESP_OK_NODATA
    );
    assert_eq!(t.map(11, 24576), RESP_OK_MAP_INFO);
    assert_eq!(t.venus.mappings(), 2);
    // Unmapping what is not mapped.
    assert_eq!(
        t.ty(&res_cmd(CMD_RESOURCE_UNMAP_BLOB, 0, 12)),
        RESP_ERR_INVALID_PARAMETER
    );
    assert_eq!(
        t.region.calls(),
        vec![
            ("place", 16384, 8192),
            ("place", 8192, 8192),
            ("withdraw", 8192, 8192),
            ("place", 24576, 8192),
        ]
    );
    // A blob made without USE_MAPPABLE cannot be mapped.
    let mut nm = ResourceCreateBlob::from_bytes(&blob_cmd(1, 13, 4096)).unwrap();
    nm.blob_flags = BLOB_FLAG_USE_SHAREABLE;
    assert_eq!(t.ty(&nm.to_bytes()), RESP_OK_NODATA);
    assert_eq!(t.map(13, 1 << 20), RESP_ERR_INVALID_PARAMETER);
}

/// Without a frontend channel nothing can be placed, and the map says so.
#[test]
fn a_map_without_a_window_is_refused() {
    let mut t = Rig::new();
    t.ctx(1);
    assert_eq!(t.blob(1, 10, 4096), RESP_OK_NODATA);
    let n = match t
        .venus
        .dispatch(&map_cmd(10, 0), &mut t.resp, Env::default())
    {
        Outcome::Done(n) => n,
        Outcome::Held(_) => panic!("held"),
    };
    assert_eq!(
        CtrlHdr::from_bytes(&t.resp[16..n]).unwrap().ty,
        RESP_ERR_UNSPEC
    );
    assert_eq!(t.venus.mappings(), 0);
}

/// Length must match the command; a message too short to be one, or too
/// long to be allowed, is a transport error.
#[test]
fn malformed_commands_are_refused() {
    let mut t = Rig::new();
    t.ctx(1);
    // A byte short of a header: the message itself is bad.
    t.send_with(&[0u8; 23], None);
    assert_eq!(status(&t.resp), -libc::EINVAL);
    // Over 4 MiB.
    let big = submit_cmd(hdr(CMD_SUBMIT_3D, 1), &vec![0u8; GPU_CMD_MAX]);
    t.send_with(&big, None);
    assert_eq!(status(&t.resp), -libc::EMSGSIZE);
    // Lengths that do not match the type.
    let mut long = map_cmd(10, 0).to_vec();
    long.push(0);
    assert_eq!(t.ty(&long), RESP_ERR_UNSPEC);
    assert_eq!(t.ty(&map_cmd(10, 0)[..39]), RESP_ERR_UNSPEC);
    let mut sub = submit_cmd(hdr(CMD_SUBMIT_3D, 1), b"abcd");
    sub.pop();
    assert_eq!(t.ty(&sub), RESP_ERR_UNSPEC);
    let mut entries = ResourceCreateBlob::from_bytes(&blob_cmd(1, 10, 4096)).unwrap();
    entries.nr_entries = 1;
    assert_eq!(t.ty(&entries.to_bytes()), RESP_ERR_UNSPEC);
    let mut with_entry = entries.to_bytes().to_vec();
    with_entry.extend_from_slice(&[0u8; 16]);
    assert_eq!(
        t.ty(&with_entry),
        RESP_ERR_INVALID_PARAMETER,
        "HOST3D has no entries"
    );
    // A command not served (RESOURCE_CREATE_2D).
    assert_eq!(t.ty(&hdr(0x0101, 0).to_bytes()), RESP_ERR_UNSPEC);
    assert!(t.r.mock.lock().unwrap().submitted.is_empty());
    // A response that does not fit is a transport error.
    let n = match t.venus.dispatch(
        &GetCapset {
            hdr: hdr(CMD_GET_CAPSET, 0),
            capset_id: CAPSET_VENUS,
            capset_version: 0,
        }
        .to_bytes(),
        &mut t.resp[..64],
        Env::default(),
    ) {
        Outcome::Done(n) => n,
        Outcome::Held(_) => panic!("held"),
    };
    assert_eq!(n, 16);
    assert_eq!(status(&t.resp), -libc::ENOSPC);
}

/// Fences complete in order per timeline, out of order across timelines,
/// and only commands that succeed with no data wait.
#[test]
fn fences_release_by_timeline() {
    let mut t = Rig::new();
    t.ctx(1);
    t.ctx(2);
    t.r.hold_fences();
    let ring = |ctx: u32, ring: u8, fence: u64| CtrlHdr {
        ty: CMD_SUBMIT_3D,
        flags: FLAG_FENCE | FLAG_INFO_RING_IDX,
        fence_id: fence,
        ctx_id: ctx,
        ring_idx: ring,
        padding: [0; 3],
    };
    let mut tokens = Vec::new();
    for h in [ring(1, 0, 1), ring(1, 0, 2), ring(1, 1, 1), ring(2, 0, 5)] {
        match t.send_with(&submit_cmd(h, b""), None) {
            Outcome::Held(tok) => tokens.push(tok),
            Outcome::Done(_) => panic!("not held"),
        }
    }
    assert_eq!(t.venus.held(), 4);
    // Only (ctx 1, ring 0) fence 2 signalled: everything submitted on that
    // ring up to it completes, in order, and nothing else.
    t.venus.fences.signal(Signalled {
        ctx_id: 1,
        ring_idx: 0,
        fence_id: 2,
    });
    let done: Vec<u64> = t.completions().iter().map(|c| c.token).collect();
    assert_eq!(done, vec![tokens[0], tokens[1]]);
    t.venus.fences.signal(Signalled {
        ctx_id: 2,
        ring_idx: 0,
        fence_id: 5,
    });
    t.venus.fences.signal(Signalled {
        ctx_id: 1,
        ring_idx: 1,
        fence_id: 1,
    });
    let done: Vec<CtrlHdr> = t
        .completions()
        .iter()
        .map(|c| CtrlHdr::from_bytes(&c.resp[16..]).unwrap())
        .collect();
    assert_eq!(done.len(), 2);
    assert_eq!((done[1].ctx_id, done[1].ring_idx, done[1].flags), (1, 1, 3));

    // A fenced command that fails is answered now, fence echoed.
    let (h, _) = t.send(&submit_cmd(fenced(CMD_SUBMIT_3D, 9, 3), b""));
    assert_eq!((h.ty, h.fence_id), (RESP_ERR_INVALID_CONTEXT_ID, 3));
    // One with data is answered now too.
    assert_eq!(t.blob(1, 10, 4096), RESP_OK_NODATA);
    let mut m = ResourceMapBlob::from_bytes(&map_cmd(10, 0)).unwrap();
    m.hdr = fenced(CMD_RESOURCE_MAP_BLOB, 0, 4);
    assert_eq!(t.send(&m.to_bytes()).0.ty, RESP_OK_MAP_INFO);
    assert_eq!(t.venus.held(), 0);
}

/// Fence ids are the guest's and need not increase: a signal completes its
/// timeline in submission order up to the first held command with that id.
#[test]
fn fences_complete_in_submission_order_not_by_id() {
    let mut t = Rig::new();
    t.ctx(1);
    t.r.hold_fences();
    let on = |ring: u8, fence: u64| CtrlHdr {
        ty: CMD_SUBMIT_3D,
        flags: FLAG_FENCE | FLAG_INFO_RING_IDX,
        fence_id: fence,
        ctx_id: 1,
        ring_idx: ring,
        padding: [0; 3],
    };
    let hold = |t: &mut Rig, h: CtrlHdr| match t.send_with(&submit_cmd(h, b""), None) {
        Outcome::Held(tok) => tok,
        Outcome::Done(_) => panic!("not held"),
    };
    let sig = |t: &mut Rig, ring: u32, fence_id: u64| {
        t.venus.fences.signal(Signalled {
            ctx_id: 1,
            ring_idx: ring,
            fence_id,
        });
        t.completions()
            .iter()
            .map(|c| c.token)
            .collect::<Vec<u64>>()
    };

    // 1000 then 0: 1000's signal does not complete 0, which is newer.
    let a = hold(&mut t, on(0, 1000));
    let b = hold(&mut t, on(0, 0));
    assert_eq!(sig(&mut t, 0, 1000), vec![a]);
    assert_eq!(sig(&mut t, 0, 0), vec![b]);

    // The same id twice on a ring: one signal each, oldest first.
    let a = hold(&mut t, on(0, 5));
    let b = hold(&mut t, on(0, 5));
    assert_eq!(sig(&mut t, 0, 5), vec![a]);
    assert_eq!(sig(&mut t, 0, 5), vec![b]);

    // Rings are independent; a signal holding nothing completes nothing.
    let a = hold(&mut t, on(0, 7));
    let b = hold(&mut t, on(1, 7));
    assert_eq!(sig(&mut t, 0, 8), Vec::<u64>::new());
    assert_eq!(sig(&mut t, 2, 7), Vec::<u64>::new());
    assert_eq!(sig(&mut t, 1, 7), vec![b]);
    assert_eq!(t.venus.held(), 1);
    assert_eq!(sig(&mut t, 0, 7), vec![a]);
    assert_eq!(t.venus.held(), 0);
}

/// A fence on a ring the renderer has no timeline for is refused before the
/// command runs or the renderer hears of it.
#[test]
fn fences_on_ring_64_and_up_are_refused() {
    let mut t = Rig::new();
    t.ctx(1);
    t.r.hold_fences();
    let mut h = fenced(CMD_SUBMIT_3D, 1, 3);
    h.flags |= FLAG_INFO_RING_IDX;
    h.ring_idx = MAX_RINGS as u8;
    let (r, _) = t.send(&submit_cmd(h, b"x"));
    assert_eq!((r.ty, r.fence_id), (RESP_ERR_INVALID_PARAMETER, 3));
    assert_eq!(t.venus.held(), 0);
    // Unfenced, ring_idx means nothing and is not checked.
    h.flags = FLAG_INFO_RING_IDX;
    assert_eq!(t.ty(&submit_cmd(h, b"x")), RESP_OK_NODATA);
    // The last ring is fine.
    h.flags = FLAG_FENCE | FLAG_INFO_RING_IDX;
    h.ring_idx = (MAX_RINGS - 1) as u8;
    assert!(matches!(
        t.send_with(&submit_cmd(h, b"x"), None),
        Outcome::Held(_)
    ));
}

/// A renderer that goes away: everything is released, held chains come back
/// as errors, and every command after is refused.
#[test]
fn a_dead_renderer_releases_everything() {
    let mut t = Rig::new();
    t.ctx(1);
    assert_eq!(t.blob(1, 10, 4096), RESP_OK_NODATA);
    assert_eq!(t.map(10, 0), RESP_OK_MAP_INFO);
    t.r.hold_fences();
    let Outcome::Held(token) = t.send_with(&submit_cmd(fenced(CMD_SUBMIT_3D, 1, 1), b""), None)
    else {
        panic!("held");
    };
    t.r.gone.store(true, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(
        t.ty(&submit_cmd(hdr(CMD_SUBMIT_3D, 1), b"")),
        RESP_ERR_UNSPEC
    );
    assert_eq!(t.region.calls().last(), Some(&("withdraw", 0, 4096)));
    assert_eq!(
        (t.venus.contexts(), t.venus.resources(), t.venus.mappings()),
        (0, 0, 0)
    );
    let done = t.completions();
    assert_eq!(done.len(), 1);
    assert_eq!(done[0].token, token);
    assert_eq!(
        CtrlHdr::from_bytes(&done[0].resp[16..]).unwrap().ty,
        RESP_ERR_UNSPEC
    );
    // Even a context the renderer never heard of is refused now.
    t.r.gone.store(false, std::sync::atomic::Ordering::Relaxed);
    let c = CtxCreate {
        hdr: hdr(CMD_CTX_CREATE, 3),
        context_init: CAPSET_VENUS,
        ..Default::default()
    };
    assert_eq!(t.ty(&c.to_bytes()), RESP_ERR_UNSPEC);
}

/// A renderer that dies between commands is found out by the fence path:
/// the held chains come back `RESP_ERR_UNSPEC` from there, the device is
/// released, and every `GpuCmd` after is refused `RESP_ERR_UNSPEC`.
#[test]
fn renderer_death_fails_held_chains() {
    let mut t = Rig::new();
    t.ctx(1);
    assert_eq!(t.blob(1, 10, 4096), RESP_OK_NODATA);
    assert_eq!(t.map(10, 0), RESP_OK_MAP_INFO);
    t.r.hold_fences();
    let mut tokens = Vec::new();
    for (ring, fence) in [(0, 1), (0, 2), (3, 1)] {
        let mut h = fenced(CMD_SUBMIT_3D, 1, fence);
        if ring != 0 {
            h.flags |= FLAG_INFO_RING_IDX;
            h.ring_idx = ring;
        }
        let Outcome::Held(token) = t.send_with(&submit_cmd(h, b""), None) else {
            panic!("a fenced submit must be held");
        };
        tokens.push(token);
    }
    assert!(t.completions().is_empty(), "the GPU is busy");
    assert_eq!(t.venus.held(), 3);

    // The renderer goes while nothing is being sent.
    t.r.gone.store(true, std::sync::atomic::Ordering::Relaxed);
    let done = t.completions();
    let mut got: Vec<u64> = done.iter().map(|c| c.token).collect();
    got.sort_unstable();
    assert_eq!(got, tokens);
    for c in &done {
        assert_eq!(
            status(&c.resp),
            0,
            "a virtio-gpu error, not a transport one"
        );
        assert_eq!(
            CtrlHdr::from_bytes(&c.resp[16..]).unwrap().ty,
            RESP_ERR_UNSPEC
        );
    }
    assert!(t.venus.is_lost());
    assert_eq!(t.venus.held(), 0);
    assert_eq!(t.region.calls().last(), Some(&("withdraw", 0, 4096)));
    assert_eq!(
        (t.venus.contexts(), t.venus.resources(), t.venus.mappings()),
        (0, 0, 0)
    );
    assert!(t.completions().is_empty(), "failed once");

    // Refused from now on, even with the renderer answering again, fenced
    // commands included: nothing is held for a fence that cannot come.
    t.r.gone.store(false, std::sync::atomic::Ordering::Relaxed);
    let c = CtxCreate {
        hdr: hdr(CMD_CTX_CREATE, 3),
        context_init: CAPSET_VENUS,
        ..Default::default()
    };
    assert_eq!(t.ty(&c.to_bytes()), RESP_ERR_UNSPEC);
    assert_eq!(
        t.ty(&submit_cmd(fenced(CMD_SUBMIT_3D, 1, 9), b"")),
        RESP_ERR_UNSPEC
    );
    assert_eq!(
        t.ty(&hdr(CMD_GET_DISPLAY_INFO, 0).to_bytes()),
        RESP_ERR_UNSPEC
    );
    assert_eq!(t.venus.held(), 0);
}

/// The renderer exports the scanout with the guest's layout: its size,
/// `strides[0]`, `offsets[0]` and format as the matching DRM fourcc, linear.
/// A new layout is a new export; the same one is not.
#[test]
fn scanout_layout_is_the_guests() {
    let mut t = Rig::new();
    let (link, _broker) = display();
    t.ctx(1);
    assert_eq!(t.blob(1, 10, 1 << 20), RESP_OK_NODATA);
    let cc = |s: &[u8; 4]| u32::from_le_bytes(*s);
    let mut s = SetScanoutBlob::from_bytes(&scanout_cmd(10, 300, 200)).unwrap();
    s.format = format::R8G8B8X8_UNORM;
    s.strides[0] = 1280;
    s.offsets[0] = 4096;
    assert_eq!(t.ty(&s.to_bytes()), RESP_OK_NODATA);
    for _ in 0..2 {
        assert_eq!(t.send_on(&flush_cmd(10), Some(&link)).0.ty, RESP_OK_NODATA);
    }
    let want = ScanoutLayout {
        width: 300,
        height: 200,
        stride: 1280,
        offset: 4096,
        fourcc: cc(b"XB24"),
    };
    assert_eq!(*t.r.exports.lock().unwrap(), vec![(10, want)]);
    assert_eq!(t.venus.exported_layout(10), Some(want));

    // Same size, another stride and format: exported again.
    s.format = format::B8G8R8A8_UNORM;
    s.strides[0] = 2048;
    assert_eq!(t.ty(&s.to_bytes()), RESP_OK_NODATA);
    assert_eq!(t.send_on(&flush_cmd(10), Some(&link)).0.ty, RESP_OK_NODATA);
    let want2 = ScanoutLayout {
        stride: 2048,
        fourcc: cc(b"AR24"),
        ..want
    };
    assert_eq!(*t.r.exports.lock().unwrap(), vec![(10, want), (10, want2)]);
    use std::sync::atomic::Ordering::Relaxed;
    assert_eq!(link.stats.sent.load(Relaxed), 3);
}

/// Every virtio-gpu scanout format reaches the renderer as the DRM fourcc of
/// the same memory layout.
#[test]
fn scanout_formats_map_to_drm_fourccs() {
    let cc = |s: &[u8; 4]| u32::from_le_bytes(*s);
    let table = [
        (format::B8G8R8A8_UNORM, cc(b"AR24")),
        (format::B8G8R8X8_UNORM, cc(b"XR24")),
        (format::A8R8G8B8_UNORM, cc(b"BA24")),
        (format::X8R8G8B8_UNORM, cc(b"BX24")),
        (format::R8G8B8A8_UNORM, cc(b"AB24")),
        (format::X8B8G8R8_UNORM, cc(b"RX24")),
        (format::A8B8G8R8_UNORM, cc(b"RA24")),
        (format::R8G8B8X8_UNORM, cc(b"XB24")),
    ];
    let mut t = Rig::new();
    let (link, _broker) = display();
    t.ctx(1);
    assert_eq!(t.blob(1, 10, 64 * 64 * 4), RESP_OK_NODATA);
    for (virtio, drm) in table {
        let mut s = SetScanoutBlob::from_bytes(&scanout_cmd(10, 64, 64)).unwrap();
        s.format = virtio;
        assert_eq!(t.ty(&s.to_bytes()), RESP_OK_NODATA, "format {virtio}");
        assert_eq!(t.send_on(&flush_cmd(10), Some(&link)).0.ty, RESP_OK_NODATA);
        let l = t.venus.exported_layout(10).unwrap();
        assert_eq!(l.fourcc, drm, "format {virtio}");
        assert_eq!(t.r.exports.lock().unwrap().last().unwrap().1, l);
    }
}

/// A format with no DRM fourcc is refused `RESP_ERR_INVALID_PARAMETER` at
/// SET_SCANOUT_BLOB, and the scanout is left as it was.
#[test]
fn unknown_scanout_formats_are_refused() {
    let mut t = Rig::new();
    t.ctx(1);
    assert_eq!(t.blob(1, 10, 64 * 64 * 4), RESP_OK_NODATA);
    for f in [0, 5, 66, 69, 122, 133, 135, u32::MAX] {
        let mut s = SetScanoutBlob::from_bytes(&scanout_cmd(10, 64, 64)).unwrap();
        s.format = f;
        assert_eq!(
            t.ty(&s.to_bytes()),
            RESP_ERR_INVALID_PARAMETER,
            "format {f}"
        );
        assert_eq!(t.venus.scanout_state(), None);
    }
    assert_eq!(t.ty(&scanout_cmd(10, 64, 64)), RESP_OK_NODATA);
    let mut s = SetScanoutBlob::from_bytes(&scanout_cmd(10, 64, 64)).unwrap();
    s.format = 5;
    assert_eq!(t.ty(&s.to_bytes()), RESP_ERR_INVALID_PARAMETER);
    assert_eq!(
        t.venus.scanout_state().map(|s| s.3),
        Some(format::B8G8R8X8_UNORM)
    );
    assert!(t.r.exports.lock().unwrap().is_empty());
}

/// Scanout geometry is checked against the blob; resource 0 turns it off.
#[test]
fn scanout_geometry_is_checked() {
    let mut t = Rig::new();
    t.ctx(1);
    assert_eq!(t.blob(1, 10, 64 * 64 * 4), RESP_OK_NODATA);
    // Bigger than the blob.
    assert_eq!(t.ty(&scanout_cmd(10, 64, 65)), RESP_ERR_INVALID_PARAMETER);
    // A format that is not a scanout format.
    let mut s = SetScanoutBlob::from_bytes(&scanout_cmd(10, 64, 64)).unwrap();
    s.format = 0;
    assert_eq!(t.ty(&s.to_bytes()), RESP_ERR_INVALID_PARAMETER);
    // The visible rectangle outside the image.
    let mut s = SetScanoutBlob::from_bytes(&scanout_cmd(10, 64, 64)).unwrap();
    s.r.x = 1;
    assert_eq!(t.ty(&s.to_bytes()), RESP_ERR_INVALID_PARAMETER);
    assert_eq!(t.ty(&scanout_cmd(10, 64, 64)), RESP_OK_NODATA);
    assert!(t.venus.scanout_state().is_some());
    let (link, _broker) = display();
    assert_eq!(
        t.send_on(&scanout_cmd(0, 0, 0), Some(&link)).0.ty,
        RESP_OK_NODATA
    );
    assert_eq!(t.venus.scanout_state(), None);
}

/// Display info with no display: one scanout, off.
#[test]
fn no_display_means_no_enabled_scanout() {
    let r = Shared::new();
    let mut v = Venus::new(Box::new(r), HOSTMEM, None);
    let mut resp = vec![0u8; 1024];
    let Outcome::Done(n) = v.dispatch(
        &hdr(CMD_GET_DISPLAY_INFO, 0).to_bytes(),
        &mut resp,
        Env::default(),
    ) else {
        panic!("held");
    };
    assert_eq!(n, 16 + RespDisplayInfo::LEN);
    let di = RespDisplayInfo::from_bytes(&resp[16..n]).unwrap();
    assert_eq!(di.hdr.ty, RESP_OK_DISPLAY_INFO);
    assert!(di.pmodes.iter().all(|p| p.enabled == 0));
}

fn get_edid(scanout: u32) -> [u8; GetEdid::LEN] {
    GetEdid {
        hdr: hdr(CMD_GET_EDID, 0),
        scanout,
        padding: 0,
    }
    .to_bytes()
}

/// `GET_EDID` through the dispatch: the whole `virtio_gpu_resp_edid`, 256
/// bytes of EDID for the configured mode and the rest zero.
#[test]
fn get_edid_describes_the_configured_display() {
    for (w, h, hz) in [
        (5120, 1440, 240),
        (3840, 2160, 144),
        (2560, 1440, 240),
        (1920, 1080, 60),
        (7680, 4320, 60),
    ] {
        let mode = DisplayMode {
            width: w,
            height: h,
            refresh_hz: hz,
        };
        let mut v = Venus::new(Box::new(Shared::new()), HOSTMEM, Some(mode));
        let mut resp = vec![0xeeu8; 4096];
        let Outcome::Done(n) = v.dispatch(&get_edid(0), &mut resp, Env::default()) else {
            panic!("held");
        };
        assert_eq!(status(&resp), 0);
        assert_eq!(n, 16 + RespEdid::LEN);
        let r = RespEdid::from_bytes(&resp[16..n]).unwrap();
        assert_eq!(r.hdr.ty, RESP_OK_EDID);
        assert_eq!((r.size, r.padding), (256, 0));
        assert_eq!(r.edid[..256], edid::edid(w, h, hz));
        assert!(r.edid[256..].iter().all(|&b| b == 0), "zero after size");
        // The DisplayID Type VII timing is the configured size.
        let le = |at: usize| u16::from_le_bytes([r.edid[at], r.edid[at + 1]]) as u32 + 1;
        let at = 128 + edid::TYPE_VII_AT;
        assert_eq!((le(at + 4), le(at + 12)), (w, h));
    }
    // No refresh known: 60.
    let mode = DisplayMode {
        width: 1920,
        height: 1080,
        refresh_hz: 0,
    };
    let mut v = Venus::new(Box::new(Shared::new()), HOSTMEM, Some(mode));
    let mut resp = vec![0u8; 4096];
    let Outcome::Done(n) = v.dispatch(&get_edid(0), &mut resp, Env::default()) else {
        panic!("held");
    };
    let r = RespEdid::from_bytes(&resp[16..n]).unwrap();
    assert_eq!(r.edid[..256], edid::edid(1920, 1080, 60));
}

/// Only scanout 0 has an EDID, only with a display, and the command is
/// exactly its struct.
#[test]
fn get_edid_refusals() {
    let mut t = Rig::new();
    assert_eq!(t.ty(&get_edid(0)), RESP_OK_EDID);
    assert_eq!(t.ty(&get_edid(1)), RESP_ERR_INVALID_SCANOUT_ID);
    assert_eq!(t.ty(&get_edid(16)), RESP_ERR_INVALID_SCANOUT_ID);
    let (h, body) = t.send(&get_edid(1));
    assert_eq!(h.ty, RESP_ERR_INVALID_SCANOUT_ID);
    assert!(body.is_empty());
    assert_eq!(t.ty(&get_edid(0)[..GetEdid::LEN - 4]), RESP_ERR_UNSPEC);
    let mut long = get_edid(0).to_vec();
    long.push(0);
    assert_eq!(t.ty(&long), RESP_ERR_UNSPEC);

    let mut v = Venus::new(Box::new(Shared::new()), HOSTMEM, None);
    let mut resp = vec![0u8; 4096];
    let Outcome::Done(n) = v.dispatch(&get_edid(0), &mut resp, Env::default()) else {
        panic!("held");
    };
    assert_eq!(n, 16 + CTRL_HDR_LEN);
    assert_eq!(
        CtrlHdr::from_bytes(&resp[16..]).unwrap().ty,
        RESP_ERR_UNSPEC
    );

    // A response buffer too small for the reply is a transport error.
    let mut small = vec![0u8; 512];
    let mut t = Rig::new();
    let Outcome::Done(n) = t.venus.dispatch(&get_edid(0), &mut small, Env::default()) else {
        panic!("held");
    };
    assert_eq!(n, 16);
    assert_eq!(status(&small), -libc::ENOSPC);
}

/// The limits per VM.
#[test]
fn contexts_are_limited() {
    let mut t = Rig::new();
    for id in 1..=MAX_CONTEXTS as u32 {
        t.ctx(id);
    }
    let c = CtxCreate {
        hdr: hdr(CMD_CTX_CREATE, MAX_CONTEXTS as u32 + 1),
        context_init: CAPSET_VENUS,
        ..Default::default()
    };
    assert_eq!(t.ty(&c.to_bytes()), RESP_ERR_OUT_OF_MEMORY);
}

/// The modifier is inferred from the blob's size: exactly the image is
/// linear; room for the rows NVIDIA pads an optimal image to is block-linear
/// with the block height the driver picks for that many rows.
#[test]
fn scanout_modifier_follows_the_blob_size() {
    use super::scanout::modifier_for;
    const BL16: u64 = 0x0300_0000_0060_6014;
    let l = |width: u32, height: u32| ScanoutLayout {
        width,
        height,
        stride: width * 4,
        offset: 0,
        fourcc: u32::from_le_bytes(*b"XR24"),
    };
    // What a Windows guest bound: 1920x1080 in 7680 * 1152 bytes.
    assert_eq!(modifier_for(&l(1920, 1080), 7680 * 1152), BL16);
    // With a 64 KiB tail, too.
    assert_eq!(modifier_for(&l(1920, 1080), 7680 * 1152 + 65536), BL16);
    // Linear: exactly the image, or rounded up to a page or 64 KiB.
    assert_eq!(modifier_for(&l(1920, 1080), 1920 * 1080 * 4), 0);
    assert_eq!(modifier_for(&l(1920, 1080), 8_323_072), 0);
    // One row short of the padding.
    assert_eq!(modifier_for(&l(1920, 1080), 7680 * 1151), 0);
    // Other sizes: 1440 pads to 1536, 2160 to 2176, 800 to 896.
    assert_eq!(modifier_for(&l(2560, 1440), 10240 * 1536), BL16);
    assert_eq!(modifier_for(&l(3840, 2160), 15360 * 2176), BL16);
    assert_eq!(modifier_for(&l(1280, 800), 5120 * 896), BL16);
    // No padding to see: 768 and 1024 are whole blocks.
    assert_eq!(modifier_for(&l(1024, 768), 4096 * 768), 0);
    assert_eq!(modifier_for(&l(1280, 1024), 5120 * 1024 * 2), 0);
    // Short images get short blocks: 40 rows are 5 GOBs, blocks of 8 (h=3).
    assert_eq!(modifier_for(&l(64, 40), 256 * 64), 0x0300_0000_0060_6013);
    // The offset counts.
    let mut o = l(1920, 1080);
    o.offset = 4096;
    assert_eq!(modifier_for(&o, 7680 * 1152), 0);
    assert_eq!(modifier_for(&o, 7680 * 1152 + 4096), BL16);
    // A stride that is not whole GOBs is not block-linear.
    let mut s = l(1920, 1080);
    s.stride = 7684;
    assert_eq!(modifier_for(&s, 1 << 30), 0);
}

/// `CONDUIT_VENUS_SCANOUT_MODIFIER` is `linear` or hex, with or without `0x`.
#[test]
fn scanout_modifier_override_parses() {
    use super::scanout::parse_modifier;
    assert_eq!(parse_modifier("linear"), Some(0));
    assert_eq!(parse_modifier(" LINEAR\n"), Some(0));
    assert_eq!(
        parse_modifier("0x0300000000606010"),
        Some(0x0300_0000_0060_6010)
    );
    assert_eq!(
        parse_modifier("300000000606015"),
        Some(0x0300_0000_0060_6015)
    );
    assert_eq!(parse_modifier("0x"), None);
    assert_eq!(parse_modifier("block-linear"), None);
}

/// The inferred modifier is the one the viewer gets, and an override wins.
#[test]
fn scanout_modifier_reaches_the_viewer() {
    let mut t = Rig::new();
    let (link, broker) = display();
    t.ctx(1);
    assert_eq!(t.blob(1, 10, 7680 * 1152), RESP_OK_NODATA);
    assert_eq!(t.blob(1, 11, 7680 * 1080), RESP_OK_NODATA);
    let attached = |broker: &OwnedFd| {
        let mut buf = vec![0u8; 64 * wire::CMD_SIZE];
        // SAFETY: a read into a local buffer.
        let n = unsafe { libc::read(broker.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        assert!(n > 0);
        buf[..n as usize]
            .chunks_exact(wire::CMD_SIZE)
            .map(|c| wire::Cmd::decode(c.try_into().unwrap()))
            .filter(|c| c.ty == wire::CMD_ATTACH)
            .map(|c| c.modifier)
            .collect::<Vec<_>>()
    };
    for (res, want) in [(10, 0x0300_0000_0060_6014), (11, 0)] {
        assert_eq!(t.ty(&scanout_cmd(res, 1920, 1080)), RESP_OK_NODATA);
        assert_eq!(t.venus.scanout_modifier(), Some(want));
        assert_eq!(t.send_on(&flush_cmd(res), Some(&link)).0.ty, RESP_OK_NODATA);
        assert_eq!(attached(&broker), vec![want]);
    }
    t.venus.forced_modifier = Some(0x0300_0000_0060_6010);
    assert_eq!(t.ty(&scanout_cmd(11, 1920, 1080)), RESP_OK_NODATA);
    assert_eq!(t.venus.scanout_modifier(), Some(0x0300_0000_0060_6010));
}

mod guest;
mod rm;

/// A reset with the display: the scanout goes off on the link, and a late
/// flush naming the old generation's scanout resource shows nothing -- not
/// while the id is unknown, and not once the new generation reuses the id
/// for a resource it never made the scanout.
#[test]
fn a_reset_turns_the_scanout_off_and_late_flushes_show_nothing() {
    use std::sync::atomic::Ordering::Relaxed;
    let mut t = Rig::new();
    let (link, _broker) = display();
    t.ctx(1);
    assert_eq!(t.blob(1, 11, 1280 * 720 * 4), RESP_OK_NODATA);
    assert_eq!(t.ty(&scanout_cmd(11, 1280, 720)), RESP_OK_NODATA);
    assert_eq!(t.send_on(&flush_cmd(11), Some(&link)).0.ty, RESP_OK_NODATA);
    assert_eq!(link.stats.sent.load(Relaxed), 1);

    t.venus.reset(None, Some(&link));
    assert_eq!(t.venus.scanout_state(), None);
    assert_eq!(t.venus.resources(), 0);

    // Late: the id is gone.
    assert_eq!(
        t.send_on(&flush_cmd(11), Some(&link)).0.ty,
        RESP_ERR_INVALID_RESOURCE_ID
    );
    // The next generation reuses the id; it is not the scanout.
    t.ctx(1);
    assert_eq!(t.blob(1, 11, 1280 * 720 * 4), RESP_OK_NODATA);
    assert_eq!(t.send_on(&flush_cmd(11), Some(&link)).0.ty, RESP_OK_NODATA);
    assert_eq!(link.stats.sent.load(Relaxed), 1, "nothing shown");
}
