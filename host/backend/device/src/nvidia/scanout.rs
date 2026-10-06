//! ScanoutFlip / ScanoutDisable (docs/SCANOUT.md): the guest names a host GEM
//! object by the drm_file that owns it and its handle there; the backend
//! exports it once as a dma-buf on that file's host descriptor and passes it
//! to the display broker. Nothing here calls NVKMS, and nothing copies.
//!
//! CursorUpdate is the same for the cursor plane: the cursor buffer is a host
//! GEM object too, exported through the same cache and handed to the broker,
//! which makes it the host pointer's image.

use super::*;
use crate::display::{DRM_IOCTL_GEM_CLOSE, DisplayLink, FlipOutcome, prime_export};
use protocol::messages::{CURSOR_MAX_DIM, CursorUpdate, IoctlReq, ScanoutFlip};
use std::sync::Arc;

impl NvidiaBackend {
    /// Give the backend a display. Without one, flips are acked and dropped.
    pub fn set_display(&mut self, link: Arc<DisplayLink>) {
        self.display = Some(link);
    }

    /// dma-bufs currently exported for scanout.
    pub fn scanout_buffers(&self) -> usize {
        self.dmabufs.len()
    }

    pub(super) fn handle_scanout_flip(&mut self, payload: &[u8], resp_buf: &mut [u8]) -> usize {
        let Some(f) = ScanoutFlip::from_bytes(payload) else {
            return self.write_error_resp(resp_buf, Status::BufferTooSmall, 0, libc::EINVAL);
        };
        if f.scanout != 0 {
            return self.write_error_resp(resp_buf, Status::InvalidDevice, 0, libc::EINVAL);
        }
        // The owner must be a render node this guest opened: a GEM handle
        // means something only on a DRM file, and only on the one that made it.
        let owner = f.owner_handle as u64;
        if !matches!(self.handle_kinds.get(&owner), Some(DeviceKind::Dri(_))) {
            return self.write_error_resp(resp_buf, Status::BadHandle, 0, libc::EBADF);
        }
        let Ok(drm_fd) = self.handles.get_raw(owner) else {
            return self.write_error_resp(resp_buf, Status::BadHandle, 0, libc::EBADF);
        };
        // Nobody is looking (no display client, or none that wants frames):
        // no export, no descriptor; the link only remembers which buffer it
        // was, for a client that attaches later.
        match self.display.as_ref() {
            None => return self.write_hdr(resp_buf, 0, 0),
            Some(link) if !link.wants_frames() && link.park(drm_fd, &f) => {
                return self.write_hdr(resp_buf, 0, 0);
            }
            Some(_) => {}
        }
        let host = &self.host;
        let dmabuf = match self
            .dmabufs
            .get_or_export(f.owner_handle, f.host_handle, || {
                prime_export(drm_fd, f.host_handle, |fd, req, arg| {
                    host.ioctl(fd, req, arg)
                })
            }) {
            Ok(fd) => fd,
            Err(e) => {
                log::warn!(
                    "scanout: PRIME export of handle {} on file {} failed: errno {e}; frame dropped",
                    f.host_handle,
                    f.owner_handle
                );
                return self.write_error_resp(resp_buf, Status::IoctlFailed, 0, e);
            }
        };
        if let Some(link) = self.display.as_ref() {
            match link.flip(dmabuf, &f) {
                FlipOutcome::Sent
                | FlipOutcome::Busy
                | FlipOutcome::NoBroker
                | FlipOutcome::Unsupported => {}
                FlipOutcome::Broken => log::debug!("scanout: broker link broke on flip {}", f.seq),
            }
        }
        self.write_hdr(resp_buf, 0, 0)
    }

    pub(super) fn handle_cursor_update(&mut self, payload: &[u8], resp_buf: &mut [u8]) -> usize {
        const AR24: u32 = 0x3432_5241;
        let Some(c) = CursorUpdate::from_bytes(payload) else {
            return self.write_error_resp(resp_buf, Status::BufferTooSmall, 0, libc::EINVAL);
        };
        if c.scanout != 0 {
            return self.write_error_resp(resp_buf, Status::InvalidDevice, 0, libc::EINVAL);
        }
        if !c.visible() {
            if let Some(link) = self.display.as_ref() {
                link.cursor(None, &c);
            }
            return self.write_hdr(resp_buf, 0, 0);
        }
        // The guest head advertises exactly this; anything else is a guest
        // bug, refused here rather than exported for the broker to refuse.
        if c.width == 0
            || c.height == 0
            || c.width > CURSOR_MAX_DIM
            || c.height > CURSOR_MAX_DIM
            || c.hot_x >= c.width
            || c.hot_y >= c.height
            || c.fourcc != AR24
        {
            return self.write_error_resp(resp_buf, Status::InvalidDevice, 0, libc::EINVAL);
        }
        let owner = c.owner_handle as u64;
        if !matches!(self.handle_kinds.get(&owner), Some(DeviceKind::Dri(_))) {
            return self.write_error_resp(resp_buf, Status::BadHandle, 0, libc::EBADF);
        }
        let Ok(drm_fd) = self.handles.get_raw(owner) else {
            return self.write_error_resp(resp_buf, Status::BadHandle, 0, libc::EBADF);
        };
        // As for a frame: nothing is exported for nobody.
        match self.display.as_ref() {
            None => return self.write_hdr(resp_buf, 0, 0),
            Some(link) if !link.wants_frames() && link.park_cursor(drm_fd, &c) => {
                return self.write_hdr(resp_buf, 0, 0);
            }
            Some(_) => {}
        }
        let host = &self.host;
        let dmabuf = match self
            .dmabufs
            .get_or_export(c.owner_handle, c.host_handle, || {
                prime_export(drm_fd, c.host_handle, |fd, req, arg| {
                    host.ioctl(fd, req, arg)
                })
            }) {
            Ok(fd) => fd,
            Err(e) => {
                log::warn!(
                    "cursor: PRIME export of handle {} on file {} failed: errno {e}",
                    c.host_handle,
                    c.owner_handle
                );
                return self.write_error_resp(resp_buf, Status::IoctlFailed, 0, e);
            }
        };
        if let Some(link) = self.display.as_ref() {
            link.cursor(Some(dmabuf), &c);
        }
        self.write_hdr(resp_buf, 0, 0)
    }

    pub(super) fn handle_scanout_disable(&mut self, _payload: &[u8], resp_buf: &mut [u8]) -> usize {
        if let Some(link) = self.display.as_ref() {
            link.disable();
        }
        self.write_hdr(resp_buf, 0, 0)
    }

    /// A forwarded `DRM_IOCTL_GEM_CLOSE`: the host may reuse the handle
    /// number, so the dma-buf exported for it goes now.
    pub(super) fn note_gem_close(&mut self, payload: &[u8]) {
        let parked = self.display.as_ref().is_some_and(|l| l.has_parked());
        if (self.dmabufs.is_empty() && !parked && self.rm_layouts.is_empty())
            || payload.len() < size_of::<IoctlReq>() + 4
        {
            return;
        }
        let req = read_struct::<IoctlReq>(payload, 0);
        if req.cmd as u64 != DRM_IOCTL_GEM_CLOSE {
            return;
        }
        let at = size_of::<IoctlReq>();
        let handle = u32::from_le_bytes(payload[at..at + 4].try_into().unwrap());
        self.dmabufs.forget(self.current_handle, handle);
        // The layout NVK imported it with, likewise: a reused number is a
        // different object.
        self.rm_layouts.forget(self.current_handle, handle);
        if let Some(link) = self.display.as_ref() {
            link.forget(self.current_handle, handle);
        }
    }

    /// The guest closed a file: everything exported from it goes.
    pub(super) fn forget_scanout_file(&mut self, owner: u64) {
        if !self.dmabufs.is_empty() {
            self.dmabufs.forget_owner(owner as u32);
        }
        self.rm_layouts.forget_owner(owner as u32);
        if let Some(link) = self.display.as_ref() {
            link.forget_owner(owner as u32);
        }
    }

    pub(super) fn teardown_scanout(&mut self) {
        self.dmabufs.clear();
        if let Some(link) = self.display.as_ref() {
            use std::sync::atomic::Ordering::Relaxed;
            let s = &link.stats;
            log::info!(
                "scanout: {} exports, {} flips sent, {} dropped busy, {} dropped with no broker, \
                 {} broken sends, {} connects, {} input events delivered, {} dropped",
                self.dmabufs.exports(),
                s.sent.load(Relaxed),
                s.busy.load(Relaxed),
                s.no_broker.load(Relaxed),
                s.broken.load(Relaxed),
                s.connects.load(Relaxed),
                s.input_events.load(Relaxed),
                s.input_dropped.load(Relaxed),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::DRM_IOCTL_PRIME_HANDLE_TO_FD;
    use protocol::messages::MsgHeader;
    use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
    use std::sync::Mutex;

    /// Answers PRIME export with a fresh memfd and counts the calls.
    #[derive(Clone, Default)]
    struct PrimeHost(Arc<Mutex<Vec<(u64, u32)>>>);
    impl HostDriver for PrimeHost {
        fn ioctl(&self, _fd: RawFd, request: u64, arg: &mut [u8]) -> std::result::Result<(), i32> {
            let h = u32::from_le_bytes(arg[0..4].try_into().unwrap());
            self.0.lock().unwrap().push((request, h));
            if request == DRM_IOCTL_PRIME_HANDLE_TO_FD {
                let fd = unsafe { libc::memfd_create(c"buf".as_ptr(), libc::MFD_CLOEXEC) };
                arg[8..12].copy_from_slice(&fd.to_le_bytes());
            }
            Ok(())
        }
    }

    fn msg(t: MsgType, handle: u32, body: &[u8]) -> Vec<u8> {
        let h = MsgHeader {
            msg_type: t as u32,
            handle,
            status: 0,
            padding: 0,
        };
        let mut v = vec![0u8; 16];
        write_struct(&mut v, &h);
        v.extend_from_slice(body);
        v
    }

    fn status(resp: &[u8]) -> i32 {
        read_struct::<MsgHeader>(resp, 0).status
    }

    fn setup_without_viewer() -> (NvidiaBackend, PrimeHost, u32) {
        let mut be = NvidiaBackend::for_test();
        let host = PrimeHost::default();
        be.set_host(Box::new(host.clone()));
        let null: OwnedFd = std::fs::File::open("/dev/null").unwrap().into();
        let h = be.handles.insert(null);
        be.handle_kinds.insert(h, DeviceKind::Dri(0));
        (be, host, h as u32)
    }

    /// With a display client attached that reads nothing (the socket keeps
    /// what the few flips here send).
    fn setup() -> (NvidiaBackend, PrimeHost, u32) {
        let (mut be, host, owner) = setup_without_viewer();
        let mut sv = [0i32; 2];
        assert_eq!(
            unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, sv.as_mut_ptr()) },
            0
        );
        let link = DisplayLink::new(None);
        link.adopt(unsafe { OwnedFd::from_raw_fd(sv[0]) });
        // The broker's end stays open (leaked) for the test's life.
        let _ = sv[1];
        be.set_display(link);
        (be, host, owner)
    }

    /// No display client, or none that wants frames: acked, and nothing is
    /// exported -- the flip costs what an unconnected display always did.
    #[test]
    fn flips_without_a_client_are_acked_and_not_exported() {
        let (mut be, host, owner) = setup_without_viewer();
        let mut resp = [0u8; 64];
        // No display at all.
        be.dispatch(&flip(owner, 7, 0), &mut resp);
        assert_eq!(status(&resp), 0);
        // A display with no client connected.
        let link = DisplayLink::new(None);
        be.set_display(link.clone());
        for seq in 1..5 {
            be.dispatch(&flip(owner, 7, seq), &mut resp);
            assert_eq!(status(&resp), 0);
        }
        be.dispatch(&cursor_msg(owner, 21, 64, 3, true), &mut resp);
        assert_eq!(status(&resp), 0);
        assert!(host.0.lock().unwrap().is_empty(), "nothing exported");
        assert_eq!(be.scanout_buffers(), 0);
        assert!(link.has_parked());
        // Closing the file forgets what was parked.
        be.dispatch(&msg(MsgType::Close, owner, &[]), &mut resp);
        assert!(!link.has_parked());
    }

    fn flip(owner: u32, handle: u32, seq: u64) -> Vec<u8> {
        msg(
            MsgType::ScanoutFlip,
            0,
            &ScanoutFlip {
                owner_handle: owner,
                host_handle: handle,
                width: 64,
                height: 64,
                stride: 256,
                fourcc: 0x3432_5258,
                seq,
                ..Default::default()
            }
            .to_bytes(),
        )
    }

    #[test]
    fn flips_export_once_and_ack_with_a_slow_client() {
        let (mut be, host, owner) = setup();
        let mut resp = [0u8; 64];
        for seq in 0..5 {
            let n = be.dispatch(&flip(owner, 7, seq), &mut resp);
            assert_eq!(n, 16);
            assert_eq!(status(&resp), 0);
            assert_eq!(
                read_struct::<MsgHeader>(&resp, 0).msg_type,
                MsgType::ScanoutFlip as u32
            );
        }
        be.dispatch(&flip(owner, 8, 5), &mut resp);
        let calls = host.0.lock().unwrap().clone();
        assert_eq!(
            calls,
            vec![
                (DRM_IOCTL_PRIME_HANDLE_TO_FD, 7),
                (DRM_IOCTL_PRIME_HANDLE_TO_FD, 8)
            ]
        );
        assert_eq!(be.scanout_buffers(), 2);

        // Disable is acked.
        be.dispatch(&msg(MsgType::ScanoutDisable, 0, &[0; 8]), &mut resp);
        assert_eq!(status(&resp), 0);
    }

    #[test]
    fn gem_close_and_file_close_drop_the_export() {
        let (mut be, host, owner) = setup();
        let mut resp = vec![0u8; 4096];
        be.dispatch(&flip(owner, 7, 0), &mut resp);
        be.dispatch(&flip(owner, 8, 1), &mut resp);
        assert_eq!(be.scanout_buffers(), 2);

        // DRM_IOCTL_GEM_CLOSE {handle 7}, forwarded on the owner's file.
        let mut body = Vec::new();
        let req = IoctlReq {
            cmd: DRM_IOCTL_GEM_CLOSE as u32,
            data_len: 8,
            ..Default::default()
        };
        let mut r = [0u8; 24];
        write_struct(&mut r, &req);
        body.extend_from_slice(&r);
        body.extend_from_slice(&7u32.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        be.dispatch(&msg(MsgType::Ioctl, owner, &body), &mut resp);
        assert_eq!(be.scanout_buffers(), 1);

        // The next flip of 7 exports again: the number may be a new object.
        be.dispatch(&flip(owner, 7, 2), &mut resp);
        let exports = host
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.0 == DRM_IOCTL_PRIME_HANDLE_TO_FD)
            .count();
        assert_eq!(exports, 3);

        be.dispatch(&msg(MsgType::Close, owner, &[]), &mut resp);
        assert_eq!(be.scanout_buffers(), 0);
    }

    #[test]
    fn a_flip_on_anything_but_a_render_node_is_refused() {
        let (mut be, host, _owner) = setup();
        let null: OwnedFd = std::fs::File::open("/dev/null").unwrap().into();
        let ctl = be.handles.insert(null);
        be.handle_kinds.insert(ctl, DeviceKind::Ctl);
        let mut resp = [0u8; 64];
        be.dispatch(&flip(ctl as u32, 7, 0), &mut resp);
        assert_eq!(status(&resp), -libc::EBADF);
        be.dispatch(&flip(999, 7, 0), &mut resp);
        assert_eq!(status(&resp), -libc::EBADF);
        // Short payload.
        be.dispatch(&msg(MsgType::ScanoutFlip, 0, &[0; 10]), &mut resp);
        assert_eq!(status(&resp), -libc::EINVAL);
        assert!(
            host.0.lock().unwrap().is_empty(),
            "nothing reached the host"
        );
    }

    fn cursor_msg(owner: u32, handle: u32, w: u32, hx: u32, visible: bool) -> Vec<u8> {
        msg(
            MsgType::CursorUpdate,
            0,
            &CursorUpdate {
                width: w,
                height: 64,
                hot_x: hx,
                hot_y: 0,
                owner_handle: owner,
                host_handle: handle,
                stride: w * 4,
                fourcc: 0x3432_5241,
                flags: if visible {
                    protocol::messages::CURSOR_F_VISIBLE
                } else {
                    0
                },
                ..Default::default()
            }
            .to_bytes(),
        )
    }

    /// A cursor is exported through the same cache as a frame, once; a hide
    /// exports nothing; a malformed one is refused before the host sees it.
    #[test]
    fn cursor_updates_export_once_and_refuse_nonsense() {
        let (mut be, host, owner) = setup();
        let mut resp = [0u8; 64];
        be.dispatch(&cursor_msg(owner, 21, 64, 3, true), &mut resp);
        assert_eq!(status(&resp), 0);
        be.dispatch(&cursor_msg(owner, 21, 64, 5, true), &mut resp);
        assert_eq!(status(&resp), 0);
        be.dispatch(&cursor_msg(owner, 0, 0, 0, false), &mut resp);
        assert_eq!(status(&resp), 0);
        assert_eq!(
            host.0.lock().unwrap().clone(),
            vec![(DRM_IOCTL_PRIME_HANDLE_TO_FD, 21)]
        );
        // Too big, hotspot outside, not a render node.
        be.dispatch(&cursor_msg(owner, 22, 512, 0, true), &mut resp);
        assert_eq!(status(&resp), -libc::EINVAL);
        be.dispatch(&cursor_msg(owner, 22, 64, 64, true), &mut resp);
        assert_eq!(status(&resp), -libc::EINVAL);
        be.dispatch(&cursor_msg(999, 22, 64, 0, true), &mut resp);
        assert_eq!(status(&resp), -libc::EBADF);
        assert_eq!(
            host.0.lock().unwrap().len(),
            1,
            "nothing more reached the host"
        );
        // And the guest closing the GEM handle drops the export, like a frame's.
        assert_eq!(be.scanout_buffers(), 1);
    }

    /// The whole backend path into a fake broker: the dma-buf the broker gets
    /// is the one PRIME export produced.
    #[test]
    fn a_flip_reaches_the_broker_with_the_exported_buffer() {
        let (mut be, _host, owner) = setup();
        let mut sv = [0i32; 2];
        assert_eq!(
            unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, sv.as_mut_ptr()) },
            0
        );
        let ours = unsafe { OwnedFd::from_raw_fd(sv[0]) };
        let broker = unsafe { OwnedFd::from_raw_fd(sv[1]) };
        let link = DisplayLink::new(None);
        link.adopt(ours);
        be.set_display(link.clone());
        let mut resp = [0u8; 64];
        be.dispatch(&flip(owner, 7, 1), &mut resp);
        assert_eq!(status(&resp), 0);
        assert_eq!(
            link.stats.sent.load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        // WINDOW + QUERY_FORMAT + ATTACH + COMMIT, 40 bytes each.
        let mut buf = [0u8; 160];
        let mut got = 0;
        while got < buf.len() {
            let n = unsafe {
                libc::recv(
                    broker.as_raw_fd(),
                    buf[got..].as_mut_ptr().cast(),
                    buf.len() - got,
                    0,
                )
            };
            assert!(n > 0);
            got += n as usize;
        }
        assert_eq!(
            u16::from_le_bytes([buf[80], buf[81]]),
            crate::display::wire::CMD_ATTACH
        );
        let _ = broker.into_raw_fd();
    }
}
