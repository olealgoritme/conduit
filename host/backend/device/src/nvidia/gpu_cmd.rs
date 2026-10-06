//! GpuCmd (docs/VENUS.md): a virtio-gpu command for the Venus renderer,
//! served by `crate::venus` when the backend runs with `--venus` and refused
//! otherwise, the way an unknown message is.

use super::*;

impl NvidiaBackend {
    pub(super) fn handle_gpu_cmd(&mut self, payload: &[u8], resp_buf: &mut [u8]) -> usize {
        #[cfg(feature = "venus")]
        if let Some(venus) = self.venus.as_mut() {
            // Field by field, so Venus can be borrowed mutably beside it.
            let rm = super::rm_import::RmView {
                handles: &self.handles,
                kinds: &self.handle_kinds,
                host: &*self.host,
                layouts: &self.rm_layouts,
            };
            let env = crate::venus::Env {
                window: self.window.as_deref(),
                display: self.display.as_deref(),
                rm: Some(&rm),
                ram: self.guest_ram.as_deref(),
            };
            return match venus.dispatch(payload, resp_buf, env) {
                crate::venus::Outcome::Done(n) => n,
                crate::venus::Outcome::Held(token) => {
                    self.venus_held = Some(token);
                    0
                }
            };
        }
        let _ = payload;
        self.write_error_resp(resp_buf, Status::InvalidMsgType, 0, 0)
    }
}

#[cfg(feature = "venus")]
impl NvidiaBackend {
    /// Serve `GpuCmd` with `venus`. Without this the device has no Venus.
    pub fn set_venus(&mut self, venus: crate::venus::Venus) {
        self.venus = Some(venus);
    }

    pub fn venus(&self) -> Option<&crate::venus::Venus> {
        self.venus.as_ref()
    }

    /// RM-export blobs are served (docs/VENUS.md "RM-export blobs"): Venus
    /// is on and its renderer imports dma-bufs. The transport offers
    /// [`protocol::messages::NVGPU_CFG_RM_IMPORT`] on it.
    pub fn venus_rm_import(&self) -> bool {
        self.venus.as_ref().is_some_and(|v| v.rm_import())
    }

    /// After [`NvidiaBackend::dispatch`]: when the message was a fenced
    /// `GpuCmd`, nothing was written, and this is the token its response
    /// will come back under from [`NvidiaBackend::venus_completions`]. The
    /// transport must keep the chain until then.
    pub fn take_held(&mut self) -> Option<u64> {
        self.venus_held.take()
    }

    /// Responses for held chains that may now be returned.
    /// A renderer found dead here releases the device as one found dead in
    /// a command does, so this needs the window and display too.
    pub fn venus_completions(&mut self) -> Vec<crate::venus::Completion> {
        let env = crate::venus::Env {
            window: self.window.as_deref(),
            display: self.display.as_deref(),
            rm: None,
            ram: None,
        };
        self.venus
            .as_mut()
            .map(|v| v.completions(env))
            .unwrap_or_default()
    }

    /// The Venus renderer is gone for good (see [`crate::venus::Venus::is_lost`]).
    pub fn venus_lost(&self) -> bool {
        self.venus.as_ref().is_some_and(|v| v.is_lost())
    }

    /// A descriptor readable when a fence may have signalled, for a
    /// transport to poll: a duplicate, so it outlives this borrow.
    pub fn venus_fence_fd(&self) -> Option<OwnedFd> {
        self.venus
            .as_ref()
            .and_then(|v| v.fence_fd().try_clone_to_owned().ok())
    }

    /// On device reset: take Venus out, emptied. The held chains' error
    /// responses are dropped -- the frontend reset the queue they were on.
    pub(super) fn reset_venus(&mut self) -> Option<crate::venus::Venus> {
        self.venus_held = None;
        let mut venus = self.venus.take()?;
        let dropped = venus.reset(None).len();
        if dropped > 0 {
            log::info!("device reset: {dropped} held Venus chain(s) went with the queue");
        }
        Some(venus)
    }

    pub(super) fn teardown_venus(&mut self) {
        if let Some(v) = self.venus.as_mut() {
            v.report();
            v.reset(None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::venus::*;

    fn gpu_cmd(body: &[u8]) -> Vec<u8> {
        let mut v = vec![0u8; size_of::<MsgHeader>()];
        write_struct(&mut v, &MsgHeader::ok(MsgType::GpuCmd, 0));
        v.extend_from_slice(body);
        v
    }

    /// Without `--venus` a `GpuCmd` is refused the way an unknown message
    /// is: -EPROTO, a bare header, nothing served.
    #[test]
    fn gpu_cmd_is_refused_without_venus() {
        let mut be = NvidiaBackend::for_test();
        let mut resp = [0u8; 128];
        let get = CtrlHdr {
            ty: CMD_GET_DISPLAY_INFO,
            ..Default::default()
        };
        let n = be.dispatch(&gpu_cmd(&get.to_bytes()), &mut resp);
        assert_eq!(n, size_of::<MsgHeader>());
        let h = read_struct::<MsgHeader>(&resp, 0);
        assert_eq!(h.status, -libc::EPROTO);
        assert_eq!(h.msg_type, MsgType::GpuCmd as u32);
    }

    /// With Venus, a fenced command leaves nothing written and a token to
    /// wait on; a reset keeps Venus, emptied.
    #[cfg(feature = "venus")]
    #[test]
    fn gpu_cmd_reaches_venus_and_fences_are_held() {
        let mut be = NvidiaBackend::for_test();
        be.set_venus(crate::venus::Venus::new(
            Box::new(conduit_venus::mock::Mock::new()),
            1 << 20,
            None,
        ));
        let mut resp = [0u8; 1024];
        let ctx = CtxCreate {
            hdr: CtrlHdr {
                ty: CMD_CTX_CREATE,
                ctx_id: 1,
                ..Default::default()
            },
            context_init: CAPSET_VENUS,
            ..Default::default()
        };
        let n = be.dispatch(&gpu_cmd(&ctx.to_bytes()), &mut resp);
        assert_eq!(n, 16 + CTRL_HDR_LEN);
        assert_eq!(read_struct::<MsgHeader>(&resp, 0).status, 0);
        assert_eq!(CtrlHdr::from_bytes(&resp[16..]).unwrap().ty, RESP_OK_NODATA);
        assert_eq!(be.take_held(), None);

        let submit = Submit3d {
            hdr: CtrlHdr {
                ty: CMD_SUBMIT_3D,
                flags: FLAG_FENCE,
                fence_id: 9,
                ctx_id: 1,
                ..Default::default()
            },
            size: 0,
            padding: 0,
        };
        assert_eq!(be.dispatch(&gpu_cmd(&submit.to_bytes()), &mut resp), 0);
        let token = be.take_held().expect("held");
        assert_eq!(be.take_held(), None, "taken once");
        let done = be.venus_completions();
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].token, token);
        assert!(be.venus_fence_fd().is_some());

        be.reset();
        let v = be.venus().expect("kept across a reset");
        assert_eq!((v.contexts(), v.held()), (0, 0));
        be.dispatch(&gpu_cmd(&ctx.to_bytes()), &mut resp);
        assert_eq!(CtrlHdr::from_bytes(&resp[16..]).unwrap().ty, RESP_OK_NODATA);
    }
}
