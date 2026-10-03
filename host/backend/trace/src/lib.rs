//! The backend's request trace.
//!
//! With tracing on, `conduit-backend` writes one [`Record`] per guest request:
//! what was asked (an open, an ioctl and which one, an mmap...), what came
//! back (errno, RM status, a refusal), and where the time went between the
//! request arriving and its reply being handed back to the guest.
//!
//! This crate is the record and everything that reads it: the two encodings
//! (a fixed-size binary record and JSON Lines), names for the numbers in it,
//! filters, the one-line rendering `conduit trace --follow` prints, and the
//! summary `conduit trace analyze` prints. It does no I/O of its own beyond
//! `std::io::Read`/`Write` and takes no locks, so the backend can use it on
//! its writer thread and the CLI can use it offline. See docs/TRACING.md.

pub mod filter;
pub mod json;
pub mod pretty;
pub mod read;
pub mod summary;

pub use abi::version::DriverVersion;

/// The first bytes of a binary trace, and of a live stream.
pub const MAGIC: [u8; 8] = *b"CNDTRACE";
/// Bumped when a record's layout changes.
pub const FORMAT_VERSION: u16 = 1;
/// A binary header: magic, format version, record length, driver release.
pub const HEADER_LEN: usize = 32;
/// One binary record.
pub const RECORD_LEN: usize = 72;

/// What kind of message the guest sent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum Kind {
    Open = 1,
    Close = 2,
    Ioctl = 3,
    Mmap = 4,
    Munmap = 5,
    /// A notification to the guest that a file it watches has an event.
    /// Travels host to guest; it has no reply and no latency.
    Event = 6,
    /// Display, cursor, clipboard and file-tree messages.
    #[default]
    Other = 7,
    /// Not a request: this many records were lost because the writer fell
    /// behind. The count is in `sub`.
    Dropped = 8,
}

/// What a request is, finer than [`Kind`]: which RM escape, which driver.
/// This is what `--filter` and the summary group by.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum Call {
    #[default]
    Other = 0,
    Open = 1,
    Close = 2,
    Mmap = 3,
    Munmap = 4,
    Event = 5,
    /// `NV_ESC_RM_ALLOC`; `sub` is the class.
    Alloc = 6,
    /// `NV_ESC_RM_CONTROL`; `sub` is the command.
    Control = 7,
    /// `NV_ESC_RM_FREE`.
    Free = 8,
    /// `NV_ESC_RM_DUP_OBJECT`.
    Dup = 9,
    /// `NV_ESC_RM_MAP_MEMORY`.
    Map = 10,
    /// `NV_ESC_RM_UNMAP_MEMORY`.
    Unmap = 11,
    /// Any other ioctl on an NVIDIA node (`/dev/nvidiactl`, `/dev/nvidiaN`).
    Rm = 12,
    /// An ioctl on `/dev/nvidia-modeset`; `sub` is the NVKMS command.
    Nvkms = 13,
    /// An ioctl on a DRM render node.
    Drm = 14,
    /// An ioctl on `/dev/nvidia-uvm`.
    Uvm = 15,
    /// Scanout flips and cursor updates.
    Display = 16,
    Clipboard = 17,
    /// The guest fetching the driver's /proc and /sys files.
    Files = 18,
    Dropped = 19,
}

/// Why the backend answered a request itself instead of forwarding it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum Refusal {
    #[default]
    None = 0,
    /// The guest's capabilities (`--caps`) do not include it.
    Caps = 1,
    /// RM does not serve this control or class to an unprivileged caller, or
    /// its parameters are not the size RM's are.
    Allowlist = 2,
    /// The escape is unknown to, or the wrong size for, the host release.
    Abi = 3,
    /// An NVKMS command that would act on the host's own display.
    HostDisplay = 4,
    /// A UVM file whose VA space allows pageable access.
    UvmPageable = 5,
    /// The allocation would take the guest over its video memory limit.
    VramLimit = 6,
    /// Answered by the backend on purpose, with success.
    Local = 7,
    /// Malformed: too short, a handle that is not open, an unknown message.
    BadRequest = 8,
}

macro_rules! names {
    ($ty:ident { $($v:ident => $s:literal),* $(,)? }) => {
        impl $ty {
            pub const ALL: &'static [$ty] = &[$($ty::$v),*];
            pub fn as_str(self) -> &'static str {
                match self { $($ty::$v => $s),* }
            }
            pub fn parse(s: &str) -> Option<Self> {
                match s { $($s => Some($ty::$v),)* _ => None }
            }
            pub fn from_u8(b: u8) -> Option<Self> {
                Self::ALL.iter().copied().find(|v| *v as u8 == b)
            }
        }
        impl std::fmt::Display for $ty {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

names!(Kind {
    Open => "open", Close => "close", Ioctl => "ioctl", Mmap => "mmap",
    Munmap => "munmap", Event => "event", Other => "other", Dropped => "dropped",
});

names!(Call {
    Other => "other", Open => "open", Close => "close", Mmap => "mmap",
    Munmap => "munmap", Event => "event", Alloc => "alloc", Control => "control",
    Free => "free", Dup => "dup", Map => "map", Unmap => "unmap", Rm => "rm",
    Nvkms => "nvkms", Drm => "drm", Uvm => "uvm", Display => "display",
    Clipboard => "clipboard", Files => "files", Dropped => "dropped",
});

names!(Refusal {
    None => "", Caps => "caps", Allowlist => "allowlist", Abi => "abi",
    HostDisplay => "host-display", UvmPageable => "uvm-pageable",
    VramLimit => "vram-limit", Local => "local", BadRequest => "bad-request",
});

/// One guest request.
///
/// Times are nanoseconds. `ts_ns` is `CLOCK_MONOTONIC` when the request was
/// taken off the virtqueue; the others are offsets from it. The host driver
/// span covers every host ioctl the request needed, from the start of the
/// first to the end of the last.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Record {
    pub ts_ns: u64,
    /// The guest's file handle (for an open, the one it was given).
    pub handle: u32,
    pub kind: Kind,
    pub call: Call,
    pub refusal: Refusal,
    /// The ioctl number as the guest sent it; for an open, the device type.
    pub nr: u32,
    /// The RM class, RM control command, NVKMS command, or dropped count.
    pub sub: Option<u32>,
    /// Bytes of parameters the guest sent, and that went back.
    pub size_in: u32,
    pub size_out: u32,
    /// The errno the guest's syscall returns; 0 for success.
    pub errno: i32,
    /// RM's own status word, for the RM calls that carry one.
    pub nv_status: Option<u32>,
    /// How many host ioctls serving it took.
    pub host_calls: u16,
    /// Offsets of the first host ioctl's start and the last one's end.
    pub host_ns: Option<(u64, u64)>,
    /// Offset of the reply being handed back to the guest.
    pub reply_ns: u64,
}

impl Record {
    /// Request received to reply handed back.
    pub fn total_ns(&self) -> u64 {
        self.reply_ns
    }

    /// Received to the first host ioctl: decoding, checks, translation.
    pub fn queue_ns(&self) -> Option<u64> {
        self.host_ns.map(|(s, _)| s)
    }

    /// Time inside the host driver.
    pub fn host_time_ns(&self) -> Option<u64> {
        self.host_ns.map(|(s, e)| e.saturating_sub(s))
    }

    /// Last host ioctl's end to the reply: copying results back, replying.
    pub fn after_host_ns(&self) -> Option<u64> {
        self.host_ns.map(|(_, e)| self.reply_ns.saturating_sub(e))
    }

    /// The guest saw a failure: an errno, a non-zero RM status, or a refusal.
    pub fn failed(&self) -> bool {
        self.errno != 0
            || self.nv_status.is_some_and(|s| s != 0)
            || !matches!(self.refusal, Refusal::None | Refusal::Local)
    }

    /// The marker the writer inserts when records were lost.
    pub fn dropped(ts_ns: u64, count: u64) -> Self {
        Record {
            ts_ns,
            kind: Kind::Dropped,
            call: Call::Dropped,
            sub: Some(count.min(u32::MAX as u64) as u32),
            ..Default::default()
        }
    }

    /// The fixed-size binary record, little-endian.
    ///
    /// ```text
    ///  0 ts_ns u64        8 handle u32      12 kind u8   13 call u8
    /// 14 refusal u8      15 flags u8        16 nr u32    20 sub u32
    /// 24 size_in u32     28 size_out u32    32 errno i32 36 nv_status u32
    /// 40 host_calls u16  42 reserved        48 host_start u64
    /// 56 host_end u64    64 reply u64
    /// ```
    /// flags: 1 = sub present, 2 = nv_status present, 4 = host span present.
    pub fn to_bytes(&self) -> [u8; RECORD_LEN] {
        let mut b = [0u8; RECORD_LEN];
        let mut flags = 0u8;
        b[0..8].copy_from_slice(&self.ts_ns.to_le_bytes());
        b[8..12].copy_from_slice(&self.handle.to_le_bytes());
        b[12] = self.kind as u8;
        b[13] = self.call as u8;
        b[14] = self.refusal as u8;
        b[16..20].copy_from_slice(&self.nr.to_le_bytes());
        if let Some(s) = self.sub {
            flags |= 1;
            b[20..24].copy_from_slice(&s.to_le_bytes());
        }
        b[24..28].copy_from_slice(&self.size_in.to_le_bytes());
        b[28..32].copy_from_slice(&self.size_out.to_le_bytes());
        b[32..36].copy_from_slice(&self.errno.to_le_bytes());
        if let Some(s) = self.nv_status {
            flags |= 2;
            b[36..40].copy_from_slice(&s.to_le_bytes());
        }
        b[40..42].copy_from_slice(&self.host_calls.to_le_bytes());
        if let Some((s, e)) = self.host_ns {
            flags |= 4;
            b[48..56].copy_from_slice(&s.to_le_bytes());
            b[56..64].copy_from_slice(&e.to_le_bytes());
        }
        b[64..72].copy_from_slice(&self.reply_ns.to_le_bytes());
        b[15] = flags;
        b
    }

    /// Read a binary record. `None` if a kind, call or refusal byte is not
    /// one this version knows -- a corrupt or newer file.
    pub fn from_bytes(b: &[u8; RECORD_LEN]) -> Option<Self> {
        let u32_at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        let u64_at = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
        let flags = b[15];
        Some(Record {
            ts_ns: u64_at(0),
            handle: u32_at(8),
            kind: Kind::from_u8(b[12])?,
            call: Call::from_u8(b[13])?,
            refusal: Refusal::from_u8(b[14])?,
            nr: u32_at(16),
            sub: (flags & 1 != 0).then(|| u32_at(20)),
            size_in: u32_at(24),
            size_out: u32_at(28),
            errno: u32_at(32) as i32,
            nv_status: (flags & 2 != 0).then(|| u32_at(36)),
            host_calls: u16::from_le_bytes([b[40], b[41]]),
            host_ns: (flags & 4 != 0).then(|| (u64_at(48), u64_at(56))),
            reply_ns: u64_at(64),
        })
    }
}

/// What a trace says about where it came from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Header {
    /// The host driver release, which NVKMS command names depend on.
    pub driver: Option<DriverVersion>,
}

impl Header {
    pub fn to_bytes(&self) -> [u8; HEADER_LEN] {
        let mut b = [0u8; HEADER_LEN];
        b[0..8].copy_from_slice(&MAGIC);
        b[8..10].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        b[10..12].copy_from_slice(&(RECORD_LEN as u16).to_le_bytes());
        if let Some(v) = self.driver {
            b[16..20].copy_from_slice(&v.major.to_le_bytes());
            b[20..24].copy_from_slice(&v.minor.to_le_bytes());
            b[24..28].copy_from_slice(&v.patch.to_le_bytes());
        }
        b
    }

    pub fn from_bytes(b: &[u8; HEADER_LEN]) -> Result<Self, String> {
        if b[0..8] != MAGIC {
            return Err("not a Conduit trace (bad magic)".into());
        }
        let ver = u16::from_le_bytes([b[8], b[9]]);
        let len = u16::from_le_bytes([b[10], b[11]]) as usize;
        if ver != FORMAT_VERSION || len != RECORD_LEN {
            return Err(format!(
                "trace format {ver} with {len}-byte records; this build reads format \
                 {FORMAT_VERSION} with {RECORD_LEN}-byte records"
            ));
        }
        let u = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        let driver = (u(16) != 0).then(|| DriverVersion::new(u(16), u(20), u(24)));
        Ok(Header { driver })
    }
}

/// What a record's numbers mean, as names where they are known.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Names {
    /// The operation: `RM_CONTROL`, `UVM_MAP_EXTERNAL_ALLOCATION`,
    /// `NVIDIA_GEM_MAP_OFFSET`, `nvidiactl`...
    pub op: Option<String>,
    /// The class, control command or NVKMS command inside it.
    pub sub: Option<&'static str>,
}

/// Name what a record's numbers can be named.
pub fn names(r: &Record, driver: Option<DriverVersion>) -> Names {
    let nr = r.nr & 0xff;
    let op = match r.call {
        Call::Alloc
        | Call::Control
        | Call::Free
        | Call::Dup
        | Call::Map
        | Call::Unmap
        | Call::Rm => abi::names::escape(nr).map(str::to_string),
        Call::Uvm => abi::names::uvm(r.nr).map(str::to_string),
        Call::Drm => abi::names::drm(nr).map(str::to_string),
        Call::Nvkms => Some("NVKMS".to_string()),
        Call::Open => Some(device_name(r.nr)),
        _ => None,
    };
    let sub = r.sub.and_then(|s| match r.call {
        Call::Alloc => abi::names::class(s),
        Call::Control => abi::names::control(s),
        Call::Nvkms => abi::names::nvkms(driver, s),
        _ => None,
    });
    Names { op, sub }
}

/// The host node an open's device type names.
pub fn device_name(device_type: u32) -> String {
    use protocol::messages::DeviceKind as D;
    match D::from_device_type(device_type) {
        Some(D::Gpu(n)) => format!("nvidia{n}"),
        Some(D::Ctl) => "nvidiactl".into(),
        Some(D::Uvm) => "nvidia-uvm".into(),
        Some(D::UvmTools) => "nvidia-uvm-tools".into(),
        Some(D::Modeset) => "nvidia-modeset".into(),
        Some(D::Dri(n)) => format!("dri{n}"),
        None => format!("device {device_type:#x}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn sample() -> Record {
        Record {
            ts_ns: 123_456_789_012,
            handle: 7,
            kind: Kind::Ioctl,
            call: Call::Control,
            refusal: Refusal::None,
            nr: 0xc020_462a,
            sub: Some(0x2080_0102),
            size_in: 32,
            size_out: 1024,
            errno: 0,
            nv_status: Some(0),
            host_calls: 1,
            host_ns: Some((1_200, 15_900)),
            reply_ns: 17_345,
        }
    }

    #[test]
    fn a_record_survives_its_binary_encoding() {
        let r = sample();
        assert_eq!(Record::from_bytes(&r.to_bytes()), Some(r));
        let bare = Record {
            sub: None,
            nv_status: None,
            host_ns: None,
            errno: -1,
            refusal: Refusal::Allowlist,
            ..r
        };
        assert_eq!(Record::from_bytes(&bare.to_bytes()), Some(bare));
    }

    #[test]
    fn an_unknown_kind_byte_is_rejected() {
        let mut b = sample().to_bytes();
        b[12] = 200;
        assert_eq!(Record::from_bytes(&b), None);
    }

    #[test]
    fn the_header_carries_the_release() {
        let h = Header {
            driver: Some(DriverVersion::new(580, 178, 4)),
        };
        assert_eq!(Header::from_bytes(&h.to_bytes()), Ok(h));
        let none = Header::default();
        assert_eq!(Header::from_bytes(&none.to_bytes()), Ok(none));
        let mut bad = h.to_bytes();
        bad[0] = b'X';
        assert!(Header::from_bytes(&bad).is_err());
    }

    #[test]
    fn latency_is_split_at_the_host_call() {
        let r = sample();
        assert_eq!(r.total_ns(), 17_345);
        assert_eq!(r.queue_ns(), Some(1_200));
        assert_eq!(r.host_time_ns(), Some(14_700));
        assert_eq!(r.after_host_ns(), Some(1_445));
    }

    #[test]
    fn numbers_get_names() {
        let n = names(&sample(), None);
        assert_eq!(n.op.as_deref(), Some("RM_CONTROL"));
        assert_eq!(n.sub, Some("NV2080_CTRL_CMD_GPU_GET_INFO_V2"));
        let open = Record {
            kind: Kind::Open,
            call: Call::Open,
            nr: protocol::messages::DEV_UVM,
            sub: None,
            ..sample()
        };
        assert_eq!(names(&open, None).op.as_deref(), Some("nvidia-uvm"));
    }

    #[test]
    fn every_enum_value_round_trips_through_its_name() {
        for k in Kind::ALL {
            assert_eq!(Kind::parse(k.as_str()), Some(*k));
        }
        for c in Call::ALL {
            assert_eq!(Call::parse(c.as_str()), Some(*c));
        }
        for r in Refusal::ALL {
            assert_eq!(Refusal::parse(r.as_str()), Some(*r));
        }
    }
}
