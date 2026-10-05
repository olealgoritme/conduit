//! OS events: which files RM delivers notifications on, and reading one back
//! (`NV_ESC_RM_GET_EVENT_DATA`).
//!
//! libnvidia (and librmclient) open a fresh control file, issue
//! `NV_ESC_ALLOC_OS_EVENT` on it naming itself, and allocate an
//! `NV01_EVENT_OS_EVENT` whose `data` is that file. RM queues each
//! notification on the file the OS event was allocated on (`nv_post_event` on
//! the event's `nvfp`) and wakes its poll; the transport turns that into an
//! `EventReady` like any other readable descriptor. The payload -- which
//! object fired, its notifier index, `info32`, `info16` -- is read with
//! `NV_ESC_RM_GET_EVENT_DATA` on that same file.
//!
//! NVOS41_PARAMETERS is `{ NvP64 pEvent; NvV32 MoreEvents; NvV32 status; }`,
//! and RM `copy_to_user`s a 16-byte `NvUnixEvent` through `pEvent`. The guest
//! module sends that buffer as the nested block, the way `RM_ALLOC` sends
//! `pAllocParms`; here it becomes a buffer of this process, the caller's
//! pointer goes back into the reply, and the event comes back as the nested
//! block only when RM wrote one.

use super::*;

/// `sizeof(NVOS41_PARAMETERS)`.
pub(super) const NVOS41_SIZE: usize = 16;
/// `sizeof(NvUnixEvent)`: hObject, NotifyIndex, info32, info16 and padding.
pub(super) const NV_UNIX_EVENT_SIZE: usize = 16;
const NVOS41_STATUS: usize = 12;

// nv_ioctl_alloc_os_event_t / nv_ioctl_free_os_event_t: hClient, hDevice, fd,
// Status.
const OS_EVENT_SIZE: usize = 16;
const OS_EVENT_CLIENT: usize = 0;
const OS_EVENT_FD: usize = 8;
const OS_EVENT_STATUS: usize = 12;

impl NvidiaBackend {
    /// Keep the record of which files hold an OS event, from what RM
    /// answered: only an allocation that succeeded made one.
    ///
    /// Keyed by the file the escape was issued on, because that is the file
    /// RM queues the notifications on. The entry is the client and the `fd`
    /// field as the guest wrote it (our handle for the file named), which is
    /// what `NV_ESC_FREE_OS_EVENT` quotes back.
    pub(super) fn note_os_events(&mut self, payload: &[u8], resp: &[u8]) {
        let req = read_struct::<IoctlReq>(payload, 0);
        let request = req.cmd as u64;
        if (request >> 8) & 0xFF != b'F' as u64 {
            return;
        }
        let escape = (request & 0xFF) as u32;
        if escape != abi::ioctl::NV_ESC_ALLOC_OS_EVENT && escape != abi::ioctl::NV_ESC_FREE_OS_EVENT
        {
            return;
        }
        let head = size_of::<MsgHeader>() + size_of::<IoctlResp>();
        if resp.len() < head + OS_EVENT_SIZE || read_struct::<MsgHeader>(resp, 0).status != 0 {
            return;
        }
        let out = &resp[head..];
        let word = |at: usize| u32::from_le_bytes(out[at..at + 4].try_into().unwrap());
        if word(OS_EVENT_STATUS) != NV_OK {
            return;
        }
        let key = (word(OS_EVENT_CLIENT), word(OS_EVENT_FD));
        let file = self.current_handle as u64;
        if escape == abi::ioctl::NV_ESC_ALLOC_OS_EVENT {
            self.os_events.entry(file).or_default().insert(key);
        } else {
            // RM frees by (client, fd) whichever file the call arrives on.
            self.os_events.retain(|_, set| {
                set.remove(&key);
                !set.is_empty()
            });
        }
    }

    /// The file is gone, and RM dropped its queue with it.
    pub(super) fn forget_os_events(&mut self, file: u64) {
        self.os_events.remove(&file);
    }

    /// Whether `file` has an OS event allocated on it.
    pub(super) fn has_os_event(&self, file: u64) -> bool {
        self.os_events.get(&file).is_some_and(|s| !s.is_empty())
    }

    /// `NV_ESC_RM_GET_EVENT_DATA`: read one queued notification.
    ///
    /// `param_in` is the 16-byte NVOS41 followed by the nested block, which
    /// must be exactly one `NvUnixEvent`. RM is called with a pointer to a
    /// buffer of ours; the guest's `pEvent` goes back in the reply, never to
    /// the host. The event comes back as the nested block only when RM's
    /// status is NV_OK -- otherwise RM wrote nothing through the pointer, and
    /// the guest must write nothing to its caller's buffer either.
    ///
    /// `hObject` is returned as RM wrote it: RM handles are not translated by
    /// this backend (the guest's client allocates in RM's own namespace), so
    /// RM's handle is the guest's.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn dispatch_get_event_data(
        &mut self,
        cookie: u64,
        host_fd: RawFd,
        request: u64,
        data_len: u32,
        nested_len: u32,
        deep_len: u32,
        param_in: &[u8],
        resp_buf: &mut [u8],
    ) -> usize {
        if data_len as usize != NVOS41_SIZE
            || nested_len as usize != NV_UNIX_EVENT_SIZE
            || deep_len != 0
            || param_in.len() != NVOS41_SIZE + NV_UNIX_EVENT_SIZE
        {
            log::warn!(
                "GET_EVENT_DATA: {data_len} + {nested_len} + {deep_len} bytes, expected \
                 {NVOS41_SIZE} + {NV_UNIX_EVENT_SIZE} + 0"
            );
            traced_refusal!(self, BadRequest);
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::EINVAL);
        }
        let file = self.current_handle as u64;
        if !self.has_os_event(file) {
            self.note_allow_refusal(
                "NV_ESC_RM_GET_EVENT_DATA".into(),
                format!("handle {file} has no OS event allocated on it"),
            );
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::EINVAL);
        }

        let mut params = param_in[..NVOS41_SIZE].to_vec();
        let caller_ptr: [u8; 8] = params[0..8].try_into().expect("16 bytes");

        // A guarded buffer, like every block RM writes through a pointer here:
        // an overrun lands on a guard page, not on this process's heap.
        let Some(mut event) = crate::guarded::GuardPool::lease(&self.guards, NV_UNIX_EVENT_SIZE)
        else {
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::ENOMEM);
        };
        let buf = event.as_mut_slice();
        buf.fill(0);
        params[0..8].copy_from_slice(&(buf.as_mut_ptr() as u64).to_le_bytes());

        if let Err(errno) = self.host.ioctl(host_fd, request, &mut params) {
            log::warn!("GET_EVENT_DATA: host ioctl failed: errno={errno}");
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, errno);
        }
        params[0..8].copy_from_slice(&caller_ptr);

        let status =
            u32::from_le_bytes(params[NVOS41_STATUS..NVOS41_STATUS + 4].try_into().unwrap());
        if status == NV_OK {
            log::debug!(
                "GET_EVENT_DATA on handle {file}: hObject={:#x} index={}",
                u32::from_le_bytes(buf[0..4].try_into().unwrap()),
                u32::from_le_bytes(buf[4..8].try_into().unwrap())
            );
            params.extend_from_slice(buf);
        }
        self.write_ioctl_resp(resp_buf, cookie, &params)
    }
}
