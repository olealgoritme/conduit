//! Serving one request while recording it for the trace (docs/TRACING.md).
//!
//! `dispatch_traced` is `dispatch` plus bookkeeping, and is only called while
//! a trace is being taken; with tracing off the transport calls `dispatch`
//! and none of this runs. Host ioctls are timed by putting a timing wrapper
//! in front of the host driver for the length of the one request, so the
//! handlers themselves carry no tracing code.

use super::*;
use conduit_trace::{Call, Kind, Record, Refusal};
use std::cell::Cell;

/// The host driver, with the first start and last end of its calls noted.
struct Timed {
    inner: Box<dyn HostDriver>,
    start: Cell<u64>,
    end: Cell<u64>,
    calls: Cell<u16>,
}

impl HostDriver for Timed {
    fn ioctl(&self, fd: RawFd, request: u64, arg: &mut [u8]) -> std::result::Result<(), i32> {
        let t0 = crate::trace::now_ns();
        if self.calls.get() == 0 {
            self.start.set(t0);
        }
        let r = self.inner.ioctl(fd, request, arg);
        self.end.set(crate::trace::now_ns());
        self.calls.set(self.calls.get().saturating_add(1));
        r
    }
}

/// Stands in for the host driver while it is being moved in and out of the
/// wrapper. Never called: both moves happen with no request in between.
struct Detached;

impl HostDriver for Detached {
    fn ioctl(&self, _: RawFd, _: u64, _: &mut [u8]) -> std::result::Result<(), i32> {
        Err(libc::ENODEV)
    }
}

/// Where RM writes its status in the parameter block of each RM call, given
/// how long the guest says that block is.
fn nv_status_offset(call: Call, data_len: usize) -> Option<usize> {
    let at = match call {
        // NVOS64 (48 bytes) or the older NVOS21 (32).
        Call::Alloc if data_len >= NVOS64_STATUS + 4 => NVOS64_STATUS,
        Call::Alloc => 28,
        Call::Control => NVOS54_STATUS,
        Call::Free => 12,  // NVOS00
        Call::Dup => 24,   // NVOS55
        Call::Map => 40,   // NVOS33
        Call::Unmap => 24, // NVOS34
        _ => return None,
    };
    (at + 4 <= data_len).then_some(at)
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

impl NvidiaBackend {
    /// What a request is, read from its bytes before it is served.
    fn describe(&self, req: &[u8]) -> Record {
        let mut r = Record::default();
        if req.len() < size_of::<MsgHeader>() {
            return r;
        }
        let hdr = read_struct::<MsgHeader>(req, 0);
        let payload = &req[size_of::<MsgHeader>()..];
        r.handle = hdr.handle;
        r.size_in = payload.len() as u32;
        let Some(t) = MsgType::from_u32(hdr.msg_type) else {
            return r;
        };
        (r.kind, r.call) = match t {
            MsgType::Open => (Kind::Open, Call::Open),
            MsgType::Close => (Kind::Close, Call::Close),
            MsgType::Ioctl => (Kind::Ioctl, Call::Rm),
            MsgType::Mmap => (Kind::Mmap, Call::Mmap),
            MsgType::Munmap => (Kind::Munmap, Call::Munmap),
            MsgType::GetProcFiles | MsgType::GetSysFiles => (Kind::Other, Call::Files),
            MsgType::ScanoutFlip | MsgType::ScanoutDisable | MsgType::CursorUpdate => {
                (Kind::Other, Call::Display)
            }
            MsgType::ClipboardToHost | MsgType::ClipboardRequest => (Kind::Other, Call::Clipboard),
            _ => (Kind::Other, Call::Other),
        };
        match t {
            MsgType::Open if payload.len() >= size_of::<OpenReq>() => {
                r.nr = read_struct::<OpenReq>(payload, 0).device_type;
            }
            MsgType::Mmap if payload.len() >= size_of::<MmapReq>() => {
                // For a mapping, "in" is how much was asked to be mapped.
                r.size_in = read_struct::<MmapReq>(payload, 0).size.min(u32::MAX as u64) as u32;
            }
            MsgType::Ioctl if payload.len() >= size_of::<IoctlReq>() => {
                let ireq = read_struct::<IoctlReq>(payload, 0);
                let data = &payload[size_of::<IoctlReq>()..];
                r.nr = ireq.cmd;
                r.size_in = ireq
                    .data_len
                    .saturating_add(ireq.nested_len)
                    .saturating_add(ireq.deep_len);
                let ty = (ireq.cmd >> 8) & 0xff;
                let escape = ireq.cmd & 0xff;
                let kind = self.handle_kinds.get(&(hdr.handle as u64));
                r.call = match kind {
                    Some(DeviceKind::Uvm) | Some(DeviceKind::UvmTools) => Call::Uvm,
                    Some(DeviceKind::Modeset) => Call::Nvkms,
                    Some(DeviceKind::Dri(_)) => Call::Drm,
                    _ if ty == b'm' as u32 => Call::Nvkms,
                    _ if ty == b'd' as u32 => Call::Drm,
                    _ => match escape {
                        abi::ioctl::NV_ESC_RM_ALLOC => Call::Alloc,
                        abi::ioctl::NV_ESC_RM_CONTROL => Call::Control,
                        abi::ioctl::NV_ESC_RM_FREE => Call::Free,
                        abi::ioctl::NV_ESC_RM_DUP_OBJECT => Call::Dup,
                        abi::ioctl::NV_ESC_RM_MAP_MEMORY => Call::Map,
                        abi::ioctl::NV_ESC_RM_UNMAP_MEMORY => Call::Unmap,
                        _ => Call::Rm,
                    },
                };
                r.sub = match r.call {
                    Call::Alloc => u32_at(data, 12), // hClass
                    Call::Control => u32_at(data, NVOS54_CMD),
                    Call::Nvkms => u32_at(data, 0),
                    _ => None,
                };
            }
            _ => {}
        }
        r
    }

    /// Serve a request as `dispatch` does, and say what happened.
    ///
    /// `t_recv` is when the transport took the request off its queue
    /// (`crate::trace::now_ns`). The record comes back without `reply_ns`,
    /// which only the transport knows: it sets that once the reply is handed
    /// back, then passes the record to `crate::trace::emit`.
    pub fn dispatch_traced(
        &mut self,
        req_buf: &[u8],
        resp_buf: &mut [u8],
        t_recv: u64,
    ) -> (usize, Record) {
        let mut rec = self.describe(req_buf);
        rec.ts_ns = t_recv;
        self.trace_refusal.set(Refusal::None);

        // Put the timing wrapper in front of the host driver for this one
        // request. Box<dyn HostDriver> cannot be downcast, so the wrapper is
        // recognised on the way out by its address.
        let inner = std::mem::replace(&mut self.host, Box::new(Detached));
        let timed: *mut Timed = Box::into_raw(Box::new(Timed {
            inner,
            start: Cell::new(0),
            end: Cell::new(0),
            calls: Cell::new(0),
        }));
        // SAFETY: `timed` came from Box::into_raw just above.
        self.host = unsafe { Box::from_raw(timed as *mut dyn HostDriver) };

        let n = self.dispatch(req_buf, resp_buf);

        let current = std::mem::replace(&mut self.host, Box::new(Detached));
        if std::ptr::addr_eq(&*current as *const dyn HostDriver, timed) {
            // SAFETY: the box holds the `Timed` allocated above (same
            // address); dropping the vtable gives back its concrete type.
            let t = unsafe { Box::from_raw(Box::into_raw(current) as *mut Timed) };
            rec.host_calls = t.calls.get();
            if rec.host_calls > 0 {
                rec.host_ns = Some((
                    t.start.get().saturating_sub(t_recv),
                    t.end.get().saturating_sub(t_recv),
                ));
            }
            self.host = t.inner;
        } else {
            // Something replaced the host driver while serving (no message
            // does today). Keep what it installed.
            self.host = current;
        }

        rec.refusal = self.trace_refusal.get();
        if n >= size_of::<MsgHeader>() {
            let hdr = read_struct::<MsgHeader>(resp_buf, 0);
            rec.errno = if hdr.status < 0 { -hdr.status } else { 0 };
            let mut body = n - size_of::<MsgHeader>();
            if rec.kind == Kind::Open && hdr.status == 0 {
                rec.handle = hdr.handle;
            }
            if rec.kind == Kind::Ioctl && hdr.status == 0 && body >= size_of::<IoctlResp>() {
                body -= size_of::<IoctlResp>();
                let ir = read_struct::<IoctlResp>(resp_buf, size_of::<MsgHeader>());
                let params = size_of::<MsgHeader>() + size_of::<IoctlResp>();
                if let Some(at) = nv_status_offset(rec.call, ir.data_len as usize) {
                    rec.nv_status = u32_at(&resp_buf[..n], params + at);
                }
            }
            rec.size_out = body as u32;
        }
        (n, rec)
    }
}
