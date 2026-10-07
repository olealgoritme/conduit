//! Cross-client hardening of forwarded RM ioctls (`HELIOS_ESCAPE_NVRM` `FORWARD`,
//! msg `Ioctl`): which RM clients a process owns, and which fields of a request name
//! something that is not its own. Pure logic, no allocation, safe under the KMD's
//! spinlock; the glue is `kmd_render/src/virtio/nvrm_harden.rs`, the design and the
//! threat model are in `docs/nvrm-escape.md` section 12.
//!
//! # The problem
//!
//! The KMD tracks backend FILE handles per owner (`nvrm_handle_owned`), but an RM
//! `Ioctl` carries a second namespace inside its payload: RM CLIENT handles
//! (`NV01_ROOT`), chosen by RM and unguessable only in the sense that they are
//! unpublished. RM resolves a client through the backend process, which serves ONE
//! guest, so every client in it is "the guest's": process A that learns (or guesses)
//! the number of process B's client can run RM controls in it, `RM_DUP_OBJECT` its
//! memory, or attach an event to its objects. Backend handles (the host's `fd`
//! numbers) have the same problem inside payload slots (`0x3d05`/`0x3d06`, the NVKMS
//! `memFd`, the `NV0005` event `data`, `REGISTER_FD`, the `fd` of `MAP_MEMORY`).
//!
//! # What this module does
//!
//! * [`ClientTable`]: per owner, the client handles RM minted for it, learned from the
//!   SUCCESSFUL reply of a forwarded `NV_ESC_RM_ALLOC` of a root class ([`root_alloc`],
//!   [`client_from_reply`]) and forgotten on a successful free of the client
//!   ([`client_free`]), on `Close` of the file it was made through, on the owner's
//!   teardown and with the transport. A client number RM mints again evicts a stale
//!   entry of ANOTHER owner, so one number never has two owners.
//! * [`judge`]: reads the request, finds every slot that names a client or a backend
//!   handle, and says whether each is the caller's ([`Verdict`]).
//!
//! Confidence is part of the verdict. A slot is `Deny`-grade only when the field is
//! known (an NVIDIA header or the host's own parser says what it is) AND the request's
//! parameter block has exactly the size that header gives; anything else (a size that
//! drifted, a block too short to hold the slot, a field the host does not translate)
//! is `Doubt`, which is counted and never refused. A legitimate single-client caller
//! (every reference is its own) is `Allow` by construction.
//!
//! What is deliberately NOT here is listed in `docs/nvrm-escape.md` section 12.

/// Clients tracked across every owner.
pub const MAX_CLIENTS: usize = 256;
/// Most clients one owner may hold (NVK makes one per `crm_client`).
pub const MAX_CLIENTS_PER_OWNER: usize = 32;

/// `MsgHeader` of a forwarded message.
const MSG_HDR: usize = 16;
/// `MsgHeader` + the 24-byte `IoctlReq`: where an `Ioctl` request's data block starts.
pub const IOCTL_HDR: usize = MSG_HDR + 24;
/// `MsgHeader` + the 12-byte `IoctlResp`: where an `Ioctl` reply's data block starts.
pub const REPLY_DATA: usize = MSG_HDR + 12;
/// Host `MsgType::Ioctl`.
const MSG_IOCTL: u32 = 3;

/// `ioctl` type bytes: `'F'` (NVIDIA RM escapes) and `'d'` (DRM, nvidia-drm).
const TYPE_RM: u32 = 0x46;
const TYPE_DRM: u32 = 0x64;

/// `device_type` of the UVM files (`256` UVM, `257` UVM tools): their ioctl numbers
/// are not RM's, and their calls are not covered (see the docs).
const DEVICE_TYPE_UVM: u32 = 256;
const DEVICE_TYPE_UVM_TOOLS: u32 = 257;

/// `NV_ESC_*` numbers (the low byte of the Linux ioctl number).
pub const NV_ESC_REGISTER_FD: u32 = 0xC9;
pub const NV_ESC_ALLOC_OS_EVENT: u32 = 0xCE;
pub const NV_ESC_FREE_OS_EVENT: u32 = 0xCF;
pub const NV_ESC_RM_ALLOC_MEMORY: u32 = 0x27;
pub const NV_ESC_RM_FREE: u32 = 0x29;
pub const NV_ESC_RM_CONTROL: u32 = 0x2A;
pub const NV_ESC_RM_ALLOC: u32 = 0x2B;
pub const NV_ESC_RM_DUP_OBJECT: u32 = 0x34;
pub const NV_ESC_RM_MAP_MEMORY: u32 = 0x4E;

/// RM escapes whose parameter block begins with the CALLER's client handle
/// (`hRoot` / `hClient`, every `NVOSxx` struct below). Checked against the owner's
/// clients. `0x54` (`ALLOC_CONTEXT_DMA2`, whose first field is an object), `0x52`
/// (`GET_EVENT_DATA`, no handle) and `0x5C`/`0x5D` (not routed by the host) are not
/// in the list: their first word is not known to be a client.
const CLIENT_AT_0: [u32; 22] = [
    0x27, // ALLOC_MEMORY (NVOS02, plus the trailing fd)
    0x28, // ALLOC_OBJECT (NVOS05)
    0x29, // FREE (NVOS00)
    0x2A, // CONTROL (NVOS54)
    0x2B, // ALLOC (NVOS21 / NVOS64)
    0x32, // CONFIG_GET (NVOS13)
    0x33, // CONFIG_SET (NVOS14)
    0x34, // DUP_OBJECT (NVOS55)
    0x35, // SHARE (NVOS57)
    0x37, // CONFIG_GET_EX
    0x38, // CONFIG_SET_EX
    0x39, // I2C_ACCESS
    0x41, // IDLE_CHANNELS (NVOS30)
    0x4A, // VID_HEAP_CONTROL (NVOS32)
    0x4D, // ACCESS_REGISTRY (NVOS38)
    0x4E, // MAP_MEMORY (NVOS33, plus the trailing fd)
    0x4F, // UNMAP_MEMORY (NVOS34)
    0x56, // ADD_VBLANK_CALLBACK
    0x57, // MAP_MEMORY_DMA (NVOS46)
    0x58, // UNMAP_MEMORY_DMA (NVOS47)
    0x59, // BIND_CONTEXT_DMA (NVOS49)
    0x5E, // UPDATE_DEVICE_MAPPING_INFO (NVOS56)
];

/// Classes whose allocation makes a client: `NV01_ROOT`, `NV01_ROOT_NON_PRIV`,
/// `NV01_ROOT_CLIENT` (the host's own list, `ROOT_CLASSES`).
const ROOT_CLASSES: [u32; 3] = [0x0, 0x1, 0x41];

const fn is_root_class(class: u32) -> bool {
    class == ROOT_CLASSES[0] || class == ROOT_CLASSES[1] || class == ROOT_CLASSES[2]
}

fn rd_u32(b: &[u8], at: usize) -> Option<u32> {
    let s = b.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes(s.try_into().ok()?))
}

// ---- the table -----------------------------------------------------------------------

/// One recorded client.
#[derive(Clone, Copy)]
struct Slot {
    /// The owner (`DeviceOwner::raw()`); 0 never names an owner.
    owner: usize,
    client: u32,
    /// The backend file handle the client was allocated through: the host closes the
    /// client when that file closes, so the entry goes with it.
    via: u32,
    /// The process the owner's device belongs to (its `hKmdProcess` token, captured by the
    /// escape that allocated the client); 0 when unknown. Read by the copy-engine Present
    /// route's `h_client` rule ([`ClientTable::process_of`]).
    process: usize,
}

const EMPTY: Slot = Slot {
    owner: 0,
    client: 0,
    via: 0,
    process: 0,
};

/// What [`ClientTable::commit`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Commit {
    /// A new entry.
    Recorded,
    /// The owner already had this client (its `via` file is updated).
    Known,
    /// Recorded, and another owner's entry of the same number was dropped: RM minted a
    /// number that owner could only hold if it missed the free.
    Evicted,
    /// Nothing was recorded: the number is not a client handle (0 or `0xFFFFFFFF`), or
    /// the table or the owner's quota is full (a reservation makes this unreachable).
    Refused,
}

/// Per-owner RM clients. Fixed capacity; a few words per entry, scanned linearly (a
/// session holds one or two entries per process).
///
/// A slot with `client == 0` and a nonzero owner is a RESERVATION: a promise to a client
/// allocation in flight. It counts against the table AND the owner's quota (so two
/// concurrent allocations of an owner at one below its quota cannot both reserve), is
/// never owned by anyone, and is turned into the client by [`commit`](Self::commit) or
/// given back by [`cancel`](Self::cancel).
///
/// The all-zero byte pattern is a valid empty table (the kernel builds it with
/// `alloc_zeroed`, without a 4 KiB temporary; a test pins it).
pub struct ClientTable {
    slots: [Slot; MAX_CLIENTS],
    /// Slots in use, reservations included: `slots[..used]` is live.
    used: usize,
}

impl ClientTable {
    pub const fn new() -> Self {
        Self {
            slots: [EMPTY; MAX_CLIENTS],
            used: 0,
        }
    }

    fn live(&self) -> &[Slot] {
        &self.slots[..self.used]
    }

    /// Clients recorded (reservations are not clients).
    pub fn len(&self) -> usize {
        self.live().iter().filter(|s| s.client != 0).count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Slots `owner` holds: clients and reservations.
    pub fn count_for(&self, owner: usize) -> usize {
        self.live().iter().filter(|s| s.owner == owner).count()
    }

    /// Whether `owner` was given `client` by RM and has not lost it. `owner == 0` and
    /// `client == 0` are never owned.
    pub fn is_client_owned_by(&self, owner: usize, client: u32) -> bool {
        owner != 0
            && client != 0
            && self
                .live()
                .iter()
                .any(|s| s.owner == owner && s.client == client)
    }

    /// The process that `client` was made for: the `hKmdProcess` token recorded with it, or
    /// `None` when the client is not recorded (hardening off, a full table, a client of the
    /// KMD's own) or its process is unknown. RM never has two live clients of one number
    /// ([`commit_in`](Self::commit_in) evicts a stale one), so at most one slot matches.
    pub fn process_of(&self, client: u32) -> Option<usize> {
        if client == 0 {
            return None;
        }
        self.live()
            .iter()
            .find(|s| s.client == client)
            .map(|s| s.process)
            .filter(|p| *p != 0)
    }

    /// Promise a slot to a client allocation about to be forwarded, so a full table
    /// refuses BEFORE the host makes a client nobody tracks. `false`: no room (table or
    /// the owner's quota, reservations in flight counted).
    pub fn reserve(&mut self, owner: usize) -> bool {
        if owner == 0 || self.used >= MAX_CLIENTS || self.count_for(owner) >= MAX_CLIENTS_PER_OWNER
        {
            return false;
        }
        self.slots[self.used] = Slot {
            owner,
            client: 0,
            via: 0,
            process: 0,
        };
        self.used += 1;
        true
    }

    /// Remove one reservation of `owner`. `false` if it had none.
    fn take_reservation(&mut self, owner: usize) -> bool {
        match self
            .live()
            .iter()
            .position(|s| s.owner == owner && s.client == 0)
        {
            Some(i) => {
                self.remove_at(i);
                true
            }
            None => false,
        }
    }

    /// Give `owner`'s promised slot back (the allocation failed or never left).
    pub fn cancel(&mut self, owner: usize) {
        self.take_reservation(owner);
    }

    /// Record `client` for `owner` after RM made it (`via` = the file it was made
    /// through). `reserved`: the caller holds a [`reserve`](Self::reserve) promise of this
    /// owner, which this consumes (success or not); without one (log-only mode records
    /// opportunistically) another request's promise is left alone.
    pub fn commit(&mut self, owner: usize, via: u32, client: u32, reserved: bool) -> Commit {
        self.commit_in(owner, 0, via, client, reserved)
    }

    /// [`commit`](Self::commit), also recording the process `owner`'s device belongs to
    /// (`process`, its `hKmdProcess` token; 0 when unknown). An owner is one device, so its
    /// process never changes; a known client whose recorded process disagrees becomes unknown
    /// (the safe direction for [`process_of`](Self::process_of)).
    pub fn commit_in(
        &mut self,
        owner: usize,
        process: usize,
        via: u32,
        client: u32,
        reserved: bool,
    ) -> Commit {
        if reserved {
            self.take_reservation(owner);
        }
        if owner == 0 || client == 0 || client == u32::MAX {
            return Commit::Refused;
        }
        let mut evicted = false;
        let mut i = 0;
        while i < self.used {
            let s = self.slots[i];
            if s.client == client {
                if s.owner == owner {
                    self.slots[i].via = via;
                    if s.process != process {
                        self.slots[i].process = 0;
                    }
                    return Commit::Known;
                }
                // RM never has two live clients of one number: the other owner's
                // entry is stale (a free whose reply was lost, a file closed behind
                // our back). Dropping it is the safe direction.
                self.remove_at(i);
                evicted = true;
                continue;
            }
            i += 1;
        }
        if self.used >= MAX_CLIENTS || self.count_for(owner) >= MAX_CLIENTS_PER_OWNER {
            return Commit::Refused;
        }
        self.slots[self.used] = Slot {
            owner,
            client,
            via,
            process,
        };
        self.used += 1;
        if evicted {
            Commit::Evicted
        } else {
            Commit::Recorded
        }
    }

    fn remove_at(&mut self, i: usize) {
        self.slots[i] = self.slots[self.used - 1];
        self.slots[self.used - 1] = EMPTY;
        self.used -= 1;
    }

    /// Forget one client of `owner` (its free succeeded, or timed out). `false` if the
    /// owner did not hold it.
    pub fn forget_client(&mut self, owner: usize, client: u32) -> bool {
        if client == 0 {
            return false;
        }
        match self
            .live()
            .iter()
            .position(|s| s.owner == owner && s.client == client)
        {
            Some(i) => {
                self.remove_at(i);
                true
            }
            None => false,
        }
    }

    /// Drop every slot matching `f`, returning how many CLIENTS (not reservations) went.
    fn drop_where(&mut self, f: impl Fn(&Slot) -> bool) -> u32 {
        let mut n = 0;
        let mut i = 0;
        while i < self.used {
            if f(&self.slots[i]) {
                if self.slots[i].client != 0 {
                    n += 1;
                }
                self.remove_at(i);
            } else {
                i += 1;
            }
        }
        n
    }

    /// Forget every client of `owner` made through backend file `via` (the file
    /// closed). Returns how many.
    pub fn forget_via(&mut self, owner: usize, via: u32) -> u32 {
        self.drop_where(|s| s.owner == owner && s.client != 0 && s.via == via)
    }

    /// Forget every client and reservation of `owner` (device destroy). Returns how many
    /// clients.
    pub fn forget_owner(&mut self, owner: usize) -> u32 {
        self.drop_where(|s| s.owner == owner)
    }

    /// Forget everything (the transport is retired). Returns how many clients. No
    /// 4 KiB temporary: this runs under the virtio spinlock.
    pub fn clear(&mut self) -> u32 {
        let n = self.len() as u32;
        for s in self.slots[..self.used].iter_mut() {
            *s = EMPTY;
        }
        self.used = 0;
        n
    }
}

impl Default for ClientTable {
    fn default() -> Self {
        Self::new()
    }
}

// ---- reading a request -----------------------------------------------------------------

/// The parts of a forwarded `Ioctl` request this module reads. `data` is the top-level
/// parameter block (`NVOSxx`), `nested` the block its pointer field names (alloc or
/// control parameters, or an NVKMS block).
#[derive(Clone, Copy)]
pub struct IoctlView<'a> {
    pub cmd: u32,
    pub data: &'a [u8],
    pub nested: &'a [u8],
}

impl<'a> IoctlView<'a> {
    /// `None` unless `req` is `MsgHeader{msg_type 3} | IoctlReq | data | nested` with
    /// both lengths inside the request (checked arithmetic: the lengths are the
    /// caller's).
    pub fn parse(req: &'a [u8]) -> Option<Self> {
        if rd_u32(req, 0)? != MSG_IOCTL {
            return None;
        }
        let cmd = rd_u32(req, 16)?;
        let data_len = rd_u32(req, 20)? as usize;
        let nested_len = rd_u32(req, 28)? as usize;
        let d1 = IOCTL_HDR.checked_add(data_len)?;
        let n1 = d1.checked_add(nested_len)?;
        Some(Self {
            cmd,
            data: req.get(IOCTL_HDR..d1)?,
            nested: req.get(d1..n1)?,
        })
    }

    fn ty(&self) -> u32 {
        (self.cmd >> 8) & 0xFF
    }

    fn nr(&self) -> u32 {
        self.cmd & 0xFF
    }
}

// ---- the verdict -----------------------------------------------------------------------

/// What a finding is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cause {
    /// The request's own client (`hRoot` / `hClient`, offset 0) is not the caller's.
    CallerClient,
    /// A cross-client slot (`hClientSrc`, `hParentClient`, ...) names a client that is
    /// not the caller's.
    ClientRef,
    /// A slot that holds a backend file handle (`fd`, `memFd`, `ctl_fd`, event
    /// `data`) names one the caller did not open.
    HandleRef,
    /// A slot could not be read: the block is shorter than the field, or has a size
    /// other than the one the layout was verified against.
    Malformed,
}

/// The outcome of [`judge`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Every reference is the caller's, or there is none.
    Allow,
    /// A reference that is not the caller's, in a field known well enough to refuse.
    Deny(Cause),
    /// Something unreadable or not verified: counted, never refused.
    Doubt(Cause),
}

#[derive(Clone, Copy)]
enum Kind {
    Client,
    Handle,
}

/// A 4-byte field at `off` of a block.
#[derive(Clone, Copy)]
struct Field {
    off: usize,
    kind: Kind,
    /// The field is not known to be what `kind` says in every release (or the host
    /// does not translate it): a finding is `Doubt`, never `Deny`.
    soft: bool,
}

const fn client(off: usize) -> Field {
    Field {
        off,
        kind: Kind::Client,
        soft: false,
    }
}

const fn handle(off: usize) -> Field {
    Field {
        off,
        kind: Kind::Handle,
        soft: false,
    }
}

/// A backend-handle slot where a number that is not the caller's handle is not known to
/// be an attack: a fence the caller waits on that the KMD took over or another process
/// shares (`SEMSURF_FENCE_WAIT`), or the `data` of an `NV01_EVENT` (class 0x05, not
/// 0x79), which RM may read as a callback cookie rather than a descriptor. Counted, never
/// refused, whoever it names.
const fn soft_handle(off: usize) -> Field {
    Field {
        off,
        kind: Kind::Handle,
        soft: true,
    }
}

/// A backend-handle slot of a control the host does not translate (the number reaches
/// RM as a descriptor of the backend process): not a handle of this table at all.
const fn untranslated(off: usize) -> Field {
    Field {
        off,
        kind: Kind::Handle,
        soft: true,
    }
}

/// An array of client handles: `count` at `count_off`, elements from `off`.
#[derive(Clone, Copy)]
struct Array {
    count_off: usize,
    off: usize,
    max: usize,
}

/// A parameter block's verified layout: its exact size (0 = any size that reaches the
/// field) and the fields in it that name something.
struct Layout {
    size: usize,
    fields: &'static [Field],
    array: Option<Array>,
}

/// Allocation classes whose parameters (`pAllocParms`, the nested block) carry a
/// reference. Sizes and offsets: the NVIDIA class headers compiled with `offsetof`,
/// and the host's own allow-list (`rmallow`, 24 / 56 / 12 / 8 across every release).
const CLASS_LAYOUTS: [(u32, Layout); 5] = [
    // NV0005_ALLOC_PARAMETERS { hParentClient, hSrcResource, hClass, notifyIndex, data }
    // (`NV01_EVENT`): the source resource's client, and the event file's handle (the
    // host turns `data`'s low word into a descriptor).
    (
        0x05,
        Layout {
            size: 24,
            fields: &[client(0), soft_handle(16)],
            array: None,
        },
    ),
    // `NV01_EVENT_OS_EVENT`: the same struct.
    (
        0x79,
        Layout {
            size: 24,
            fields: &[client(0), handle(16)],
            array: None,
        },
    ),
    // NV0080_ALLOC_PARAMETERS { deviceId, hClientShare, hTargetClient, hTargetDevice, .. }
    (
        0x80,
        Layout {
            size: 56,
            fields: &[client(4), client(8)],
            array: None,
        },
    ),
    // NV83DE_ALLOC_PARAMETERS { hDebuggerClient_Obsolete, hAppClient, hClass3dObject }:
    // the debuggee's client.
    (
        0x83DE,
        Layout {
            size: 12,
            fields: &[client(4)],
            array: None,
        },
    ),
    // NVB2CC_ALLOC_PARAMETERS { hClientTarget, hContextTarget } (0 = none).
    (
        0xB2CC,
        Layout {
            size: 8,
            fields: &[client(0)],
            array: None,
        },
    ),
];

/// `NV2080_CTRL_FIFO_DISABLE_CHANNELS_MAX_ENTRIES`.
const DISABLE_CHANNELS_MAX: usize = 64;

/// `RM_CONTROL` commands whose parameters carry a reference, from the NVIDIA control
/// headers (`offsetof`) intersected with the host's allow-list (a control the host
/// refuses needs no entry). Sizes are the allow-list's, equal in every release but
/// where 0 says the field sits in a prefix that is the same whatever the release adds.
const CONTROL_LAYOUTS: [(u32, Layout); 13] = [
    // NV0000_CTRL_CMD_OS_UNIX_EXPORT_OBJECT_TO_FD: { object, fd @16, flags }
    (
        0x0000_3D05,
        Layout {
            size: 24,
            fields: &[handle(16)],
            array: None,
        },
    ),
    // NV0000_CTRL_CMD_OS_UNIX_IMPORT_OBJECT_FROM_FD: { fd @0, object }
    (
        0x0000_3D06,
        Layout {
            size: 20,
            fields: &[handle(0)],
            array: None,
        },
    ),
    // The fd-carrying siblings the host does NOT translate (it forwards the number as
    // it is, which RM reads as a descriptor of the backend process). Counted so a
    // workload that uses one shows up; the fix is host-side.
    (
        0x0000_3D08, // GET_EXPORT_OBJECT_INFO { fd @0 }
        Layout {
            size: 0,
            fields: &[untranslated(0)],
            array: None,
        },
    ),
    (
        0x0000_3D0A, // CREATE_EXPORT_OBJECT_FD { .., fd @72 }
        Layout {
            size: 0,
            fields: &[untranslated(72)],
            array: None,
        },
    ),
    (
        0x0000_3D0B, // EXPORT_OBJECTS_TO_FD { fd @0, .. }
        Layout {
            size: 0,
            fields: &[untranslated(0)],
            array: None,
        },
    ),
    (
        0x0000_3D0C, // IMPORT_OBJECTS_FROM_FD { fd @0, .. }
        Layout {
            size: 0,
            fields: &[untranslated(0)],
            array: None,
        },
    ),
    // NV0000_CTRL_CMD_CLIENT_GET_ACCESS_RIGHTS { hObject, hClient, maskSize }
    (
        0x0000_0D03,
        Layout {
            size: 12,
            fields: &[client(4)],
            array: None,
        },
    ),
    // NV2080_CTRL_CMD_DMA_INVALIDATE_TLB { hClient, hDevice, ... }
    (
        0x2080_2502,
        Layout {
            size: 16,
            fields: &[client(0)],
            array: None,
        },
    ),
    // NV2080_CTRL_CMD_FIFO_DISABLE_CHANNELS { .., numChannels @4, .., hClientList @24 }
    (
        0x2080_110B,
        Layout {
            size: 536,
            fields: &[],
            array: Some(Array {
                count_off: 4,
                off: 24,
                max: DISABLE_CHANNELS_MAX,
            }),
        },
    ),
    // NV208F_CTRL_CMD_FIFO_GET_CHANNEL_STATE { .., hClient @4, .. }
    (
        0x208F_0403,
        Layout {
            size: 16,
            fields: &[client(4)],
            array: None,
        },
    ),
    // NV503C_CTRL_CMD_REGISTER_PID { hClient }
    (
        0x503C_0106,
        Layout {
            size: 4,
            fields: &[client(0)],
            array: None,
        },
    ),
    // NVA084_CTRL_CMD_BIND_FECS_EVTBUF { hEventBufferClient, .. }
    (
        0xA084_0105,
        Layout {
            size: 16,
            fields: &[client(0)],
            array: None,
        },
    ),
    // NV2080_CTRL_CMD_GPU_EXEC_REG_OPS { hClientTarget (0 = all channels), .. }
    (
        0x2080_0122,
        Layout {
            size: 48,
            fields: &[client(0)],
            array: None,
        },
    ),
];

/// The GR context-switch binds: `hClient` of the channel's owner. The preemption bind
/// is 104 or 112 bytes by release, so no size is asserted (offset 4 is in the prefix).
const CONTROL_LAYOUTS_GR: [(u32, Layout); 4] = [
    (
        0x2080_1209, // CTXSW_PM_BIND { hClient @0, .. }
        Layout {
            size: 40,
            fields: &[client(0)],
            array: None,
        },
    ),
    (
        0x2080_1211, // CTXSW_PREEMPTION_BIND { .., hClient @4, .. }
        Layout {
            size: 0,
            fields: &[client(4)],
            array: None,
        },
    ),
    (
        0x2080_1208, // CTXSW_ZCULL_BIND { hClient @0, .. }
        Layout {
            size: 24,
            fields: &[client(0)],
            array: None,
        },
    ),
    (
        0x2080_1205, // CTXSW_ZCULL_MODE { .., hShareClient @4, .. }
        Layout {
            size: 16,
            fields: &[client(4)],
            array: None,
        },
    ),
];

/// Collects the strongest finding of one request: a `Deny` outranks a `Doubt`, the
/// first of each kind is the one reported.
struct Acc {
    deny: Option<Cause>,
    doubt: Option<Cause>,
}

impl Acc {
    const fn new() -> Self {
        Self {
            deny: None,
            doubt: None,
        }
    }

    fn deny(&mut self, c: Cause) {
        if self.deny.is_none() {
            self.deny = Some(c);
        }
    }

    fn doubt(&mut self, c: Cause) {
        if self.doubt.is_none() {
            self.doubt = Some(c);
        }
    }

    fn verdict(&self) -> Verdict {
        match (self.deny, self.doubt) {
            (Some(c), _) => Verdict::Deny(c),
            (None, Some(c)) => Verdict::Doubt(c),
            (None, None) => Verdict::Allow,
        }
    }
}

/// Everything a check needs to resolve a reference.
struct Ctx<'a, F: Fn(u32) -> bool> {
    table: &'a ClientTable,
    owner: usize,
    handle_owned: F,
}

impl<F: Fn(u32) -> bool> Ctx<'_, F> {
    /// A client handle is the caller's, or absent (0: RM refuses it itself).
    fn client_ok(&self, v: u32) -> bool {
        v == 0 || self.table.is_client_owned_by(self.owner, v)
    }

    /// A backend handle slot, read as the host reads it (an `i32`): negative is "none"
    /// (`-1` is the caller saying it has no file), 0 is no handle, else it must be one
    /// the caller opened.
    fn handle_ok(&self, v: u32) -> bool {
        let s = v as i32;
        s <= 0 || (self.handle_owned)(v)
    }

    /// Check the 4-byte field `f` of `block`. `exact`: the block's size is the one the
    /// layout was verified against (only then can a finding be a `Deny`).
    fn field(&self, acc: &mut Acc, block: &[u8], f: Field, exact: bool, deny: Cause) {
        let Some(v) = rd_u32(block, f.off) else {
            acc.doubt(Cause::Malformed);
            return;
        };
        let ok = match f.kind {
            Kind::Client => self.client_ok(v),
            Kind::Handle => self.handle_ok(v),
        };
        if ok {
            return;
        }
        if exact && !f.soft {
            acc.deny(deny);
        } else {
            acc.doubt(deny);
        }
    }

    /// A [`Layout`] over `block`. An empty block means "no parameters": no reference.
    fn layout(&self, acc: &mut Acc, block: &[u8], l: &Layout) {
        if block.is_empty() {
            return;
        }
        let exact = l.size == 0 || block.len() == l.size;
        for f in l.fields {
            let cause = match f.kind {
                Kind::Client => Cause::ClientRef,
                Kind::Handle => Cause::HandleRef,
            };
            self.field(acc, block, *f, exact, cause);
        }
        if let Some(a) = l.array {
            let Some(count) = rd_u32(block, a.count_off) else {
                acc.doubt(Cause::Malformed);
                return;
            };
            let count = (count as usize).min(a.max);
            for i in 0..count {
                self.field(acc, block, client(a.off + 4 * i), exact, Cause::ClientRef);
            }
        }
    }

    /// The request's own client at the start of `data`.
    fn caller(&self, acc: &mut Acc, data: &[u8]) {
        self.field(acc, data, client(0), true, Cause::CallerClient);
        // `field` reports an unreadable word as a doubt; a block that cannot even hold
        // the client of an RM escape is not one RM accepts.
    }
}

/// Judge one forwarded `Ioctl` request of `owner`, on a backend file of `device_type`.
///
/// `table` is the owner's clients, `handle_owned(h)` says whether `owner` opened backend
/// handle `h`. `req` is the whole message (`MsgHeader | IoctlReq | data | nested ..`);
/// one that does not parse is `Allow` here (the length check that precedes the real
/// forward refuses it).
pub fn judge<F: Fn(u32) -> bool>(
    table: &ClientTable,
    owner: usize,
    device_type: u32,
    req: &[u8],
    handle_owned: F,
) -> Verdict {
    if device_type == DEVICE_TYPE_UVM || device_type == DEVICE_TYPE_UVM_TOOLS {
        return Verdict::Allow;
    }
    let Some(io) = IoctlView::parse(req) else {
        return Verdict::Allow;
    };
    let cx = Ctx {
        table,
        owner,
        handle_owned,
    };
    let mut acc = Acc::new();
    match io.ty() {
        TYPE_RM => rm_escape(&cx, &mut acc, &io),
        TYPE_DRM => drm_ioctl(&cx, &mut acc, &io),
        _ => {}
    }
    acc.verdict()
}

fn rm_escape<F: Fn(u32) -> bool>(cx: &Ctx<'_, F>, acc: &mut Acc, io: &IoctlView<'_>) {
    let nr = io.nr();
    match nr {
        // `NV_ESC_REGISTER_FD`: nv_ioctl_register_fd_t { int ctl_fd }.
        NV_ESC_REGISTER_FD => {
            cx.field(
                acc,
                io.data,
                handle(0),
                io.data.len() == 4,
                Cause::HandleRef,
            );
        }
        // nv_ioctl_alloc_os_event_t { hClient, hDevice, fd, Status } (alloc and free).
        NV_ESC_ALLOC_OS_EVENT | NV_ESC_FREE_OS_EVENT => {
            cx.caller(acc, io.data);
            cx.field(
                acc,
                io.data,
                handle(8),
                io.data.len() == 16,
                Cause::HandleRef,
            );
        }
        NV_ESC_RM_ALLOC => rm_alloc(cx, acc, io),
        NV_ESC_RM_CONTROL => rm_control(cx, acc, io),
        NV_ESC_RM_DUP_OBJECT => {
            // NVOS55_PARAMETERS { hClient, hParent, hObject, hClientSrc, hObjectSrc,
            // flags, status } (28 bytes). The object is looked up in hClientSrc, so
            // the client is the reference to check; `hObjectSrc` follows from it.
            cx.caller(acc, io.data);
            cx.field(
                acc,
                io.data,
                client(12),
                io.data.len() == 28,
                Cause::ClientRef,
            );
        }
        // NVOS02 (56 bytes with the trailing fd) and NVOS33 (56 with fd): the file the
        // mapping context is made on.
        NV_ESC_RM_ALLOC_MEMORY | NV_ESC_RM_MAP_MEMORY => {
            cx.caller(acc, io.data);
            cx.field(
                acc,
                io.data,
                handle(48),
                io.data.len() == 56,
                Cause::HandleRef,
            );
        }
        _ => {
            if CLIENT_AT_0.contains(&nr) {
                cx.caller(acc, io.data);
            }
        }
    }
}

fn rm_alloc<F: Fn(u32) -> bool>(cx: &Ctx<'_, F>, acc: &mut Acc, io: &IoctlView<'_>) {
    // NVOS21 / NVOS64: hRoot, hObjectParent, hObjectNew, hClass.
    let Some(class) = rd_u32(io.data, 12) else {
        acc.doubt(Cause::Malformed);
        return;
    };
    // A client allocation names no client yet (RM picks the number).
    if is_root_class(class) {
        return;
    }
    cx.caller(acc, io.data);
    for (c, l) in CLASS_LAYOUTS.iter() {
        if *c == class {
            cx.layout(acc, io.nested, l);
        }
    }
}

fn rm_control<F: Fn(u32) -> bool>(cx: &Ctx<'_, F>, acc: &mut Acc, io: &IoctlView<'_>) {
    cx.caller(acc, io.data);
    // NVOS54_PARAMETERS { hClient, hObject, cmd, flags, params, paramsSize, status }.
    let Some(cmd) = rd_u32(io.data, 8) else {
        acc.doubt(Cause::Malformed);
        return;
    };
    for (c, l) in CONTROL_LAYOUTS.iter().chain(CONTROL_LAYOUTS_GR.iter()) {
        if *c == cmd {
            cx.layout(acc, io.nested, l);
        }
    }
}

/// The NVKMS block of a GEM import / export begins with `int memFd` (any size).
const NVKMS_MEM_FD: Layout = Layout {
    size: 0,
    fields: &[handle(0)],
    array: None,
};

/// `NvKmsKapiPrivImportSemaphoreSurfaceParams { hClient, hSemaphoreSurface, size }`.
const NVKMS_IMPORT_SEMSURF: Layout = Layout {
    size: 0,
    fields: &[client(0)],
    array: None,
};

fn drm_ioctl<F: Fn(u32) -> bool>(cx: &Ctx<'_, F>, acc: &mut Acc, io: &IoctlView<'_>) {
    match io.nr() {
        // `DRM_COMMAND_BASE + DRM_NVIDIA_GEM_IMPORT_NVKMS_MEMORY / EXPORT_NVKMS_MEMORY /
        // EXPORT_DMABUF_MEMORY`: the NVKMS block the nested pointer names begins with
        // `int memFd`.
        0x41 | 0x49 | 0x4D => {
            cx.layout(acc, io.nested, &NVKMS_MEM_FD);
            if io.nested.is_empty() {
                // The host refuses a block with no `memFd`; nothing is named here.
                acc.doubt(Cause::Malformed);
            }
        }
        // `SEMSURF_FENCE_CTX_CREATE`: the nested block is
        // `NvKmsKapiPrivImportSemaphoreSurfaceParams { hClient, hSemaphoreSurface, size }`.
        0x54 => {
            cx.layout(acc, io.nested, &NVKMS_IMPORT_SEMSURF);
            if io.nested.is_empty() {
                acc.doubt(Cause::Malformed);
            }
        }
        // `SEMSURF_FENCE_WAIT` (24 bytes): { ctx, fd @4, pre_wait_value, post_wait_value };
        // `fd` is a fence handle (0 = already signalled, which `handle_ok` lets by). Doubt,
        // never Deny: the fence may be one the KMD took over from its creator or one another
        // process shares, and nothing here says that cannot be a legitimate wait.
        0x56 => {
            cx.field(
                acc,
                io.data,
                soft_handle(4),
                io.data.len() == 24,
                Cause::HandleRef,
            );
        }
        _ => {}
    }
}

// ---- learning clients from replies ----------------------------------------------------

/// Where RM's status word sits in an `NV_ESC_RM_ALLOC` data block of `data_len`: 28
/// (NVOS21, 32 bytes) or 40 (NVOS64, 48 bytes); `None` for any other size, which RM
/// itself refuses.
const fn alloc_status_off(data_len: usize) -> Option<usize> {
    match data_len {
        32 => Some(28),
        48 => Some(40),
        _ => None,
    }
}

/// Whether `req` is a client allocation (`NV_ESC_RM_ALLOC` of a root class, in a block
/// size RM accepts): the one request whose reply makes a client.
pub fn root_alloc(req: &[u8]) -> bool {
    let Some(io) = IoctlView::parse(req) else {
        return false;
    };
    io.ty() == TYPE_RM
        && io.nr() == NV_ESC_RM_ALLOC
        && alloc_status_off(io.data.len()).is_some()
        && rd_u32(io.data, 12).is_some_and(is_root_class)
}

/// The client a successful [`root_alloc`] reply made: `MsgHeader.status` 0, RM's own
/// status word 0, a handle that is neither 0 nor `0xFFFFFFFF`. `n` bytes of `resp` are
/// valid. `None` for anything else (RM said no, or the reply is too short to know).
pub fn client_from_reply(req: &[u8], resp: &[u8], n: usize) -> Option<u32> {
    if !root_alloc(req) {
        return None;
    }
    let io = IoctlView::parse(req)?;
    let status_off = alloc_status_off(io.data.len())?;
    let resp = resp.get(..n)?;
    // MsgHeader { msg_type, handle, status @8 }.
    if rd_u32(resp, 8)? != 0 {
        return None;
    }
    if n < REPLY_DATA.checked_add(io.data.len())? {
        return None;
    }
    if rd_u32(resp, REPLY_DATA + status_off)? != 0 {
        return None;
    }
    match rd_u32(resp, REPLY_DATA + 8)? {
        0 | u32::MAX => None,
        h => Some(h),
    }
}

/// The client `req` frees, if it is `NV_ESC_RM_FREE` of the client itself
/// (`hObjectOld == hRoot`, the 16-byte NVOS00): the free of anything else leaves the
/// client alone.
pub fn client_free(req: &[u8]) -> Option<u32> {
    let io = IoctlView::parse(req)?;
    if io.ty() != TYPE_RM || io.nr() != NV_ESC_RM_FREE || io.data.len() != 16 {
        return None;
    }
    let root = rd_u32(io.data, 0)?;
    let old = rd_u32(io.data, 8)?;
    (root != 0 && root == old).then_some(root)
}

/// Whether a reply of `n` valid bytes says the free worked: the host status and RM's
/// `NVOS00.status` (data + 12) are both 0.
pub fn free_reply_ok(resp: &[u8], n: usize) -> bool {
    let Some(resp) = resp.get(..n) else {
        return false;
    };
    n >= REPLY_DATA + 16 && rd_u32(resp, 8) == Some(0) && rd_u32(resp, REPLY_DATA + 12) == Some(0)
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec;
    use std::vec::Vec;

    const A: usize = 0x1000;
    const B: usize = 0x2000;
    const CA: u32 = 0xC1D0_0001;
    const CB: u32 = 0xC1D0_0002;
    const CTL: u32 = 255;
    const DRM: u32 = 512;

    fn ioc(ty: u32, nr: u32) -> u32 {
        // _IOWR(type, nr, size): direction bits and a size that the module must ignore.
        (3 << 30) | (0x30 << 16) | (ty << 8) | nr
    }

    /// `MsgHeader | IoctlReq | data | nested`.
    fn req(cmd: u32, data: &[u8], nested: &[u8]) -> Vec<u8> {
        let mut v = vec![0u8; IOCTL_HDR];
        v[0..4].copy_from_slice(&3u32.to_le_bytes());
        v[16..20].copy_from_slice(&cmd.to_le_bytes());
        v[20..24].copy_from_slice(&(data.len() as u32).to_le_bytes());
        v[28..32].copy_from_slice(&(nested.len() as u32).to_le_bytes());
        v.extend_from_slice(data);
        v.extend_from_slice(nested);
        v
    }

    fn words(w: &[u32]) -> Vec<u8> {
        w.iter().flat_map(|x| x.to_le_bytes()).collect()
    }

    fn table() -> ClientTable {
        let mut t = ClientTable::new();
        assert_eq!(t.commit(A, 1, CA, false), Commit::Recorded);
        assert_eq!(t.commit(B, 2, CB, false), Commit::Recorded);
        t
    }

    /// `judge` for owner A, who has opened backend handles 1 and 7 only.
    fn judge_a(t: &ClientTable, dt: u32, r: &[u8]) -> Verdict {
        judge(t, A, dt, r, |h| h == 1 || h == 7)
    }

    // ---- table ----

    #[test]
    fn two_owners_with_identical_handle_numbers_never_both_own_one() {
        let mut t = ClientTable::new();
        assert_eq!(t.commit(A, 1, 0x100, false), Commit::Recorded);
        assert!(t.is_client_owned_by(A, 0x100));
        // RM hands the same number to B: A missed the free, A's entry goes.
        assert_eq!(t.commit(B, 2, 0x100, false), Commit::Evicted);
        assert!(!t.is_client_owned_by(A, 0x100));
        assert!(t.is_client_owned_by(B, 0x100));
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn handle_reuse_after_free_does_not_resurrect_ownership() {
        let mut t = table();
        assert!(t.forget_client(A, CA));
        assert!(!t.is_client_owned_by(A, CA));
        // B is minted A's old number.
        assert_eq!(t.commit(B, 3, CA, false), Commit::Recorded);
        assert!(t.is_client_owned_by(B, CA));
        assert!(!t.is_client_owned_by(A, CA));
        // A freeing a number it no longer holds is not a free of B's.
        assert!(!t.forget_client(A, CA));
        assert!(t.is_client_owned_by(B, CA));
    }

    #[test]
    fn a_free_names_the_owners_client_only() {
        let mut t = table();
        assert!(!t.forget_client(A, CB), "A cannot forget B's client");
        assert!(t.is_client_owned_by(B, CB));
    }

    #[test]
    fn recommit_updates_the_file_not_the_count() {
        let mut t = ClientTable::new();
        assert_eq!(t.commit(A, 1, 5, false), Commit::Recorded);
        assert_eq!(t.commit(A, 9, 5, false), Commit::Known);
        assert_eq!(t.len(), 1);
        assert_eq!(t.forget_via(A, 1), 0, "the file was replaced");
        assert_eq!(t.forget_via(A, 9), 1);
        assert!(t.is_empty());
    }

    #[test]
    fn close_of_a_file_drops_only_that_owners_clients_made_through_it() {
        let mut t = ClientTable::new();
        t.commit(A, 1, 0x10, false);
        t.commit(A, 1, 0x11, false);
        t.commit(A, 2, 0x12, false);
        t.commit(B, 1, 0x20, false); // another owner's handle 1
        assert_eq!(t.forget_via(A, 1), 2);
        assert!(!t.is_client_owned_by(A, 0x10));
        assert!(!t.is_client_owned_by(A, 0x11));
        assert!(t.is_client_owned_by(A, 0x12));
        assert!(
            t.is_client_owned_by(B, 0x20),
            "same file number, other owner"
        );
    }

    #[test]
    fn owner_retire_drops_everything_of_that_owner_and_nobody_elses() {
        let mut t = table();
        t.commit(A, 1, 0x77, false);
        assert_eq!(t.forget_owner(A), 2);
        assert_eq!(t.count_for(A), 0);
        assert!(t.is_client_owned_by(B, CB));
        assert_eq!(t.clear(), 1);
        assert!(!t.is_client_owned_by(B, CB));
        assert!(t.is_empty());
    }

    /// The copy-engine route's `h_client` rule reads the process recorded with a client.
    #[test]
    fn a_client_names_the_process_it_was_made_for() {
        const P: usize = 0xffff_8000_1234_0000;
        const Q: usize = 0xffff_8000_5678_0000;
        let mut t = ClientTable::new();
        assert!(t.reserve(A));
        assert_eq!(t.process_of(0), None);
        assert_eq!(t.commit_in(A, P, 1, 0x10, true), Commit::Recorded);
        assert_eq!(t.process_of(0x10), Some(P));
        // A reservation names no client and no process.
        assert!(t.reserve(B));
        assert_eq!(t.process_of(0), None);
        t.cancel(B);
        // The old entry point records no process: unknown, never someone's.
        assert_eq!(t.commit(B, 1, 0x20, false), Commit::Recorded);
        assert_eq!(t.process_of(0x20), None);
        // RM minting 0x10 again for another owner evicts the stale entry and its process.
        assert_eq!(t.commit_in(B, Q, 2, 0x10, false), Commit::Evicted);
        assert_eq!(t.process_of(0x10), Some(Q));
        // A known client whose process disagrees becomes unknown.
        assert_eq!(t.commit_in(B, P, 2, 0x10, false), Commit::Known);
        assert_eq!(t.process_of(0x10), None);
        // Forgetting the owner forgets the process.
        assert_eq!(t.commit_in(A, P, 1, 0x30, false), Commit::Recorded);
        assert_eq!(t.forget_owner(A), 1);
        assert_eq!(t.process_of(0x30), None);
    }

    #[test]
    fn clear_forgets_reservations_too() {
        let mut t = ClientTable::new();
        for _ in 0..MAX_CLIENTS_PER_OWNER {
            assert!(t.reserve(A));
            t.cancel(A);
        }
        assert!(t.reserve(A));
        assert_eq!(t.clear(), 0, "a reservation is not a client");
        // Back to a clean quota.
        for i in 0..MAX_CLIENTS_PER_OWNER {
            assert!(t.reserve(A), "slot {i}");
            assert_eq!(t.commit(A, 1, 100 + i as u32, true), Commit::Recorded);
        }
        assert!(!t.reserve(A));
        assert_eq!(t.clear(), MAX_CLIENTS_PER_OWNER as u32);
        assert!(t.is_empty());
        assert!(t.reserve(A));
    }

    #[test]
    fn an_all_zero_table_is_an_empty_table() {
        // `new_client_table` builds the kernel's table with `alloc_zeroed` (no 4 KiB
        // temporary): the zero pattern must be the empty table.
        let fresh = ClientTable::new();
        let n = core::mem::size_of::<ClientTable>();
        // SAFETY: a plain-old-data struct without padding (usize / u32 fields only), read as
        // bytes while it is alive and unchanged.
        let bytes =
            unsafe { core::slice::from_raw_parts(&fresh as *const ClientTable as *const u8, n) };
        assert!(bytes.iter().all(|b| *b == 0));
        assert_eq!(
            n,
            MAX_CLIENTS * core::mem::size_of::<Slot>() + core::mem::size_of::<usize>()
        );
    }

    #[test]
    fn quotas_hold_per_owner_and_in_total() {
        let mut t = ClientTable::new();
        for i in 0..MAX_CLIENTS_PER_OWNER {
            assert!(t.reserve(A));
            assert_eq!(t.commit(A, 1, 1 + i as u32, true), Commit::Recorded);
        }
        assert!(!t.reserve(A), "per-owner quota");
        assert!(t.reserve(B), "another owner is not held up");
        t.cancel(B);
        // Fill the rest of the table with other owners.
        let mut o = 100usize;
        let mut h = 1000u32;
        while t.len() < MAX_CLIENTS {
            if t.count_for(o) >= MAX_CLIENTS_PER_OWNER {
                o += 1;
            }
            assert!(t.reserve(o));
            h += 1;
            assert_eq!(t.commit(o, 1, h, true), Commit::Recorded);
        }
        assert!(!t.reserve(B), "total quota");
    }

    #[test]
    fn reservations_in_flight_count_against_the_table_and_the_owners_quota() {
        // The table: 255 reservations of other owners and one client fill it.
        let mut t = ClientTable::new();
        t.commit(A, 1, 1, false);
        for i in 0..MAX_CLIENTS - 1 {
            assert!(t.reserve(1000 + i), "reservation {i}");
        }
        assert!(!t.reserve(B), "reservations count against the total");
        t.cancel(1000);
        assert!(t.reserve(B));

        // The owner's quota: one below it, two concurrent root allocations cannot both reserve.
        let mut t = ClientTable::new();
        for i in 0..MAX_CLIENTS_PER_OWNER - 1 {
            assert_eq!(t.commit(A, 1, 10 + i as u32, false), Commit::Recorded);
        }
        assert!(t.reserve(A), "the first takes the last slot");
        assert!(
            !t.reserve(A),
            "the second must be refused, not both admitted"
        );
        assert!(t.reserve(B), "another owner is not held up");
        t.cancel(B);
        // The first one finishes: its reservation becomes the client and the quota is full.
        assert_eq!(t.commit(A, 1, 99, true), Commit::Recorded);
        assert_eq!(t.count_for(A), MAX_CLIENTS_PER_OWNER);
        assert!(!t.reserve(A));
        // A failed allocation gives its slot back; a stranger's cancel takes nothing of A's.
        assert!(t.forget_client(A, 99));
        assert!(t.reserve(A));
        t.cancel(B);
        assert!(!t.reserve(A), "B's cancel did not release A's reservation");
        t.cancel(A);
        assert!(t.reserve(A));
    }

    #[test]
    fn a_reservation_is_nobodys_client_and_is_dropped_with_its_owner() {
        let mut t = ClientTable::new();
        assert!(t.reserve(A));
        assert!(!t.is_client_owned_by(A, 0));
        assert_eq!(t.len(), 0);
        assert!(
            !t.forget_client(A, 0),
            "client 0 is not a reservation handle"
        );
        assert_eq!(t.forget_via(A, 0), 0);
        assert_eq!(t.forget_owner(A), 0, "no client went");
        assert_eq!(t.count_for(A), 0, "but the reservation did");
        // A commit that lost its reservation (owner torn down in flight) still records.
        assert_eq!(t.commit(A, 1, 5, true), Commit::Recorded);
    }

    #[test]
    fn commit_refuses_what_is_not_a_client_handle() {
        let mut t = ClientTable::new();
        assert_eq!(t.commit(A, 1, 0, false), Commit::Refused);
        assert_eq!(t.commit(A, 1, u32::MAX, false), Commit::Refused);
        assert_eq!(t.commit(0, 1, 5, false), Commit::Refused);
        assert!(t.is_empty());
        assert!(!t.is_client_owned_by(A, 0));
        assert!(!t.is_client_owned_by(0, 5));
    }

    // ---- reading a request ----

    #[test]
    fn parse_refuses_what_does_not_fit_and_survives_hostile_lengths() {
        let ok = req(ioc(0x46, 0x2A), &[0; 32], &[0; 8]);
        assert!(IoctlView::parse(&ok).is_some());
        for cut in 0..ok.len() {
            // Never panics; short requests either do not parse or parse inside bounds.
            if let Some(v) = IoctlView::parse(&ok[..cut]) {
                assert!(IOCTL_HDR + v.data.len() + v.nested.len() <= cut);
            }
        }
        // u32::MAX lengths overflow nothing and read nothing.
        let mut h = req(ioc(0x46, 0x2A), &[0; 8], &[]);
        h[20..24].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(IoctlView::parse(&h).is_none());
        let mut h = req(ioc(0x46, 0x2A), &[0; 8], &[]);
        h[28..32].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(IoctlView::parse(&h).is_none());
        // Not an Ioctl.
        let mut h = ok.clone();
        h[0..4].copy_from_slice(&2u32.to_le_bytes());
        assert!(IoctlView::parse(&h).is_none());
    }

    // ---- the caller's own client ----

    fn ctl(client: u32, obj: u32, cmd: u32, nested: &[u8]) -> Vec<u8> {
        let mut d = words(&[client, obj, cmd, 0, 0, 0, nested.len() as u32, 0]);
        d.truncate(32);
        req(ioc(0x46, 0x2A), &d, nested)
    }

    #[test]
    fn a_control_in_someone_elses_client_is_refused() {
        let t = table();
        assert_eq!(
            judge_a(&t, CTL, &ctl(CA, 4, 0x1234_5678, &[])),
            Verdict::Allow
        );
        assert_eq!(
            judge_a(&t, CTL, &ctl(CB, 4, 0x1234_5678, &[])),
            Verdict::Deny(Cause::CallerClient)
        );
        // A number nobody holds: not the caller's either.
        assert_eq!(
            judge_a(&t, CTL, &ctl(0xDEAD, 4, 0x1234_5678, &[])),
            Verdict::Deny(Cause::CallerClient)
        );
        // Client 0 is RM's to refuse.
        assert_eq!(
            judge_a(&t, CTL, &ctl(0, 4, 0x1234_5678, &[])),
            Verdict::Allow
        );
    }

    #[test]
    fn every_listed_escape_checks_its_first_word() {
        let t = table();
        for nr in CLIENT_AT_0 {
            // Escapes with trailing fds need their full block to be exact; the first
            // word is checked whatever the size.
            let own = req(ioc(0x46, nr), &words(&[CA, 0, 0, 0x2080]), &[]);
            let foreign = req(ioc(0x46, nr), &words(&[CB, 0, 0, 0x2080]), &[]);
            let v_own = judge_a(&t, CTL, &own);
            assert!(
                !matches!(v_own, Verdict::Deny(_)),
                "nr {nr:#x} refused its own client: {v_own:?}"
            );
            assert_eq!(
                judge_a(&t, CTL, &foreign),
                Verdict::Deny(Cause::CallerClient),
                "nr {nr:#x}"
            );
        }
    }

    #[test]
    fn escapes_that_name_no_client_are_left_alone() {
        let t = table();
        for nr in [0x52u32, 0x54, 0x5C, 0x5D, 0xC8, 0xD2, 0x01, 0xFF] {
            let r = req(ioc(0x46, nr), &words(&[CB, 0, 0, 0]), &[]);
            assert_eq!(judge_a(&t, CTL, &r), Verdict::Allow, "nr {nr:#x}");
        }
    }

    #[test]
    fn an_escape_too_short_for_its_client_word_is_a_doubt_not_a_crash() {
        let t = table();
        for len in 0..4 {
            let r = req(ioc(0x46, 0x29), &vec![0u8; len], &[]);
            assert_eq!(
                judge_a(&t, CTL, &r),
                Verdict::Doubt(Cause::Malformed),
                "len {len}"
            );
        }
    }

    #[test]
    fn other_ioctl_namespaces_and_uvm_files_are_not_interpreted() {
        let t = table();
        let r = req(
            ioc(0x46, 0x2A),
            &words(&[CB, 0, 0x1234, 0, 0, 0, 0, 0]),
            &[],
        );
        assert_eq!(judge_a(&t, 256, &r), Verdict::Allow, "UVM file");
        assert_eq!(judge_a(&t, 257, &r), Verdict::Allow, "UVM tools file");
        let other = req(
            ioc(0x6D, 0x2A),
            &words(&[CB, 0, 0x1234, 0, 0, 0, 0, 0]),
            &[],
        );
        assert_eq!(judge_a(&t, 258, &other), Verdict::Allow, "NVKMS namespace");
        // A plain UVM command number (no ioctl type at all).
        let uvm = req(0x3000_0001, &[0; 8], &[]);
        assert_eq!(judge_a(&t, CTL, &uvm), Verdict::Allow);
    }

    // ---- NV01_ROOT allocation ----

    fn alloc21(root: u32, parent: u32, new: u32, class: u32, nested: &[u8]) -> Vec<u8> {
        let d = words(&[root, parent, new, class, 0, 0, nested.len() as u32, 0]);
        req(ioc(0x46, 0x2B), &d, nested)
    }

    fn alloc64(root: u32, parent: u32, new: u32, class: u32, nested: &[u8]) -> Vec<u8> {
        let mut d = words(&[
            root,
            parent,
            new,
            class,
            0,
            0,
            0,
            0,
            nested.len() as u32,
            0,
            0,
            0,
        ]);
        d.truncate(48);
        req(ioc(0x46, 0x2B), &d, nested)
    }

    #[test]
    fn a_root_alloc_names_no_client_and_is_recognised() {
        let t = ClientTable::new();
        for class in [0u32, 1, 0x41] {
            for r in [alloc21(0, 0, 0, class, &[]), alloc64(0, 0, 0, class, &[])] {
                assert!(root_alloc(&r), "class {class:#x}");
                assert_eq!(judge_a(&t, CTL, &r), Verdict::Allow);
            }
        }
        // Not roots.
        assert!(!root_alloc(&alloc21(CA, CA, 9, 0x80, &[])));
        assert!(!root_alloc(&req(
            ioc(0x46, 0x2A),
            &words(&[0, 0, 0, 0, 0, 0, 0, 0]),
            &[]
        )));
        assert!(!root_alloc(&req(
            ioc(0x46, 0x2B),
            &words(&[0, 0, 0, 0]),
            &[]
        )));
    }

    fn reply(host_status: i32, data: &[u8]) -> Vec<u8> {
        let mut v = vec![0u8; REPLY_DATA];
        v[0..4].copy_from_slice(&3u32.to_le_bytes());
        v[8..12].copy_from_slice(&host_status.to_le_bytes());
        v[16..20].copy_from_slice(&(data.len() as u32).to_le_bytes());
        v.extend_from_slice(data);
        v
    }

    #[test]
    fn the_new_client_is_read_from_a_successful_reply_only() {
        let q21 = alloc21(0, 0, 0, 0x41, &[]);
        let q64 = alloc64(0, 0, 0, 0x41, &[]);
        // NVOS21 reply: status at 28.
        let mut d21 = words(&[0, 0, CA, 0x41, 0, 0, 0, 0]);
        let r = reply(0, &d21);
        assert_eq!(client_from_reply(&q21, &r, r.len()), Some(CA));
        // RM status nonzero: no client.
        d21[28..32].copy_from_slice(&0x1Fu32.to_le_bytes());
        let r = reply(0, &d21);
        assert_eq!(client_from_reply(&q21, &r, r.len()), None);
        // NVOS64 reply: status at 40.
        let mut d64 = words(&[0, 0, CB, 0x41, 0, 0, 0, 0, 0, 0, 0, 0]);
        let r = reply(0, &d64);
        assert_eq!(client_from_reply(&q64, &r, r.len()), Some(CB));
        d64[40..44].copy_from_slice(&0x1Fu32.to_le_bytes());
        let r = reply(0, &d64);
        assert_eq!(client_from_reply(&q64, &r, r.len()), None);
        // Host said no.
        let r = reply(-22, &words(&[0, 0, CA, 0x41, 0, 0, 0, 0]));
        assert_eq!(client_from_reply(&q21, &r, r.len()), None);
        // Handle 0 / 0xFFFFFFFF are not clients.
        for bad in [0u32, u32::MAX] {
            let r = reply(0, &words(&[0, 0, bad, 0x41, 0, 0, 0, 0]));
            assert_eq!(client_from_reply(&q21, &r, r.len()), None);
        }
    }

    #[test]
    fn a_truncated_reply_never_makes_a_client_and_never_panics() {
        let q21 = alloc21(0, 0, 0, 0x41, &[]);
        let q64 = alloc64(0, 0, 0, 0x41, &[]);
        let ok21 = reply(0, &words(&[0, 0, CA, 0x41, 0, 0, 0, 0]));
        let ok64 = reply(0, &words(&[0, 0, CA, 0x41, 0, 0, 0, 0, 0, 0, 0, 0]));
        for (q, ok) in [(&q21, &ok21), (&q64, &ok64)] {
            for n in 0..ok.len() {
                assert_eq!(client_from_reply(q, ok, n), None, "n={n}");
            }
            assert_eq!(client_from_reply(q, ok, ok.len()), Some(CA));
            // `n` past the buffer is not trusted either.
            assert_eq!(client_from_reply(q, ok, ok.len() + 1), None);
        }
        // A request that is not a root alloc learns nothing.
        let other = alloc21(CA, CA, 9, 0x80, &[]);
        assert_eq!(client_from_reply(&other, &ok21, ok21.len()), None);
        // An odd block size (RM refuses it) learns nothing.
        let mut odd = alloc21(0, 0, 0, 0x41, &[]);
        odd[20..24].copy_from_slice(&40u32.to_le_bytes());
        odd.extend_from_slice(&[0; 8]);
        assert_eq!(client_from_reply(&odd, &ok21, ok21.len()), None);
    }

    #[test]
    fn freeing_the_client_is_recognised_and_freeing_an_object_is_not() {
        let own = req(ioc(0x46, 0x29), &words(&[CA, 0, CA, 0]), &[]);
        assert_eq!(client_free(&own), Some(CA));
        let obj = req(ioc(0x46, 0x29), &words(&[CA, CA, 0x55, 0]), &[]);
        assert_eq!(client_free(&obj), None);
        let zero = req(ioc(0x46, 0x29), &words(&[0, 0, 0, 0]), &[]);
        assert_eq!(client_free(&zero), None);
        let wrong_size = req(ioc(0x46, 0x29), &words(&[CA, 0, CA, 0, 0]), &[]);
        assert_eq!(client_free(&wrong_size), None);
        let other_nr = req(ioc(0x46, 0x2A), &words(&[CA, 0, CA, 0]), &[]);
        assert_eq!(client_free(&other_nr), None);
        let drm = req(ioc(0x64, 0x29), &words(&[CA, 0, CA, 0]), &[]);
        assert_eq!(client_free(&drm), None);
    }

    #[test]
    fn a_free_reply_needs_both_statuses_zero_and_all_the_bytes() {
        let ok = reply(0, &words(&[CA, 0, CA, 0]));
        assert!(free_reply_ok(&ok, ok.len()));
        for n in 0..ok.len() {
            assert!(!free_reply_ok(&ok, n), "n={n}");
        }
        assert!(!free_reply_ok(&ok, ok.len() + 1));
        assert!(!free_reply_ok(
            &reply(-9, &words(&[CA, 0, CA, 0])),
            REPLY_DATA + 16
        ));
        assert!(!free_reply_ok(
            &reply(0, &words(&[CA, 0, CA, 0x22])),
            REPLY_DATA + 16
        ));
    }

    // ---- RM_DUP_OBJECT ----

    fn dup(client: u32, src_client: u32, len: usize) -> Vec<u8> {
        let mut d = words(&[client, 0x10, 0x20, src_client, 0x30, 0, 0]);
        d.resize(len, 0);
        req(ioc(0x46, 0x34), &d, &[])
    }

    #[test]
    fn dup_object_from_another_owners_client_is_refused() {
        let t = table();
        // Own to own: the legitimate dup (NVK dups within one client or across two of its own).
        assert_eq!(judge_a(&t, CTL, &dup(CA, CA, 28)), Verdict::Allow);
        // Source is B's.
        assert_eq!(
            judge_a(&t, CTL, &dup(CA, CB, 28)),
            Verdict::Deny(Cause::ClientRef)
        );
        // Destination is B's: the caller's own client check fires first.
        assert_eq!(
            judge_a(&t, CTL, &dup(CB, CA, 28)),
            Verdict::Deny(Cause::CallerClient)
        );
        // Both B's.
        assert_eq!(
            judge_a(&t, CTL, &dup(CB, CB, 28)),
            Verdict::Deny(Cause::CallerClient)
        );
    }

    #[test]
    fn dup_between_two_clients_of_one_owner_is_allowed() {
        let mut t = table();
        t.commit(A, 1, 0xC1D0_0099, false);
        assert_eq!(judge_a(&t, CTL, &dup(0xC1D0_0099, CA, 28)), Verdict::Allow);
        assert_eq!(judge_a(&t, CTL, &dup(CA, 0xC1D0_0099, 28)), Verdict::Allow);
    }

    #[test]
    fn dup_with_an_unexpected_size_is_only_a_doubt() {
        let t = table();
        // RM wants exactly 28 bytes; a different size is not the layout the slot was
        // verified in.
        assert_eq!(
            judge_a(&t, CTL, &dup(CA, CB, 32)),
            Verdict::Doubt(Cause::ClientRef)
        );
        // Too short to hold hClientSrc at all.
        assert_eq!(
            judge_a(&t, CTL, &dup(CA, CB, 14)),
            Verdict::Doubt(Cause::Malformed)
        );
        assert_eq!(judge_a(&t, CTL, &dup(CA, CA, 32)), Verdict::Allow);
    }

    #[test]
    fn dup_after_the_sources_owner_retires_is_refused() {
        let mut t = table();
        t.forget_owner(B);
        // B's number is now nobody's: still not the caller's.
        assert_eq!(
            judge_a(&t, CTL, &dup(CA, CB, 28)),
            Verdict::Deny(Cause::ClientRef)
        );
    }

    // ---- fds and backend handles ----

    #[test]
    fn register_fd_names_an_owned_control_file() {
        let t = table();
        let mk = |fd: i32| req(ioc(0x46, NV_ESC_REGISTER_FD), &fd.to_le_bytes(), &[]);
        assert_eq!(judge_a(&t, 258, &mk(1)), Verdict::Allow);
        assert_eq!(
            judge_a(&t, 258, &mk(99)),
            Verdict::Deny(Cause::HandleRef),
            "someone else's file"
        );
        assert_eq!(judge_a(&t, 258, &mk(-1)), Verdict::Allow, "none");
        assert_eq!(judge_a(&t, 258, &mk(0)), Verdict::Allow, "no handle is 0");
        // A block that is not nv_ioctl_register_fd_t: unverified.
        let r = req(ioc(0x46, NV_ESC_REGISTER_FD), &words(&[99, 0]), &[]);
        assert_eq!(judge_a(&t, 258, &r), Verdict::Doubt(Cause::HandleRef));
    }

    #[test]
    fn os_events_check_the_client_and_the_file() {
        let t = table();
        for nr in [NV_ESC_ALLOC_OS_EVENT, NV_ESC_FREE_OS_EVENT] {
            let mk = |c: u32, fd: u32| req(ioc(0x46, nr), &words(&[c, 7, fd, 0]), &[]);
            assert_eq!(judge_a(&t, CTL, &mk(CA, 7)), Verdict::Allow);
            assert_eq!(
                judge_a(&t, CTL, &mk(CA, 99)),
                Verdict::Deny(Cause::HandleRef)
            );
            assert_eq!(
                judge_a(&t, CTL, &mk(CB, 7)),
                Verdict::Deny(Cause::CallerClient)
            );
        }
    }

    #[test]
    fn alloc_memory_and_map_memory_check_their_trailing_fd() {
        let t = table();
        for nr in [NV_ESC_RM_ALLOC_MEMORY, NV_ESC_RM_MAP_MEMORY] {
            let mk = |c: u32, fd: i32| {
                let mut d = words(&[c; 12]);
                d.extend_from_slice(&fd.to_le_bytes());
                d.extend_from_slice(&[0; 4]);
                assert_eq!(d.len(), 56);
                req(ioc(0x46, nr), &d, &[])
            };
            assert_eq!(judge_a(&t, 0, &mk(CA, 7)), Verdict::Allow);
            assert_eq!(judge_a(&t, 0, &mk(CA, -1)), Verdict::Allow, "no file");
            assert_eq!(judge_a(&t, 0, &mk(CA, 99)), Verdict::Deny(Cause::HandleRef));
            assert_eq!(
                judge_a(&t, 0, &mk(CB, 7)),
                Verdict::Deny(Cause::CallerClient)
            );
            // Not the 56-byte shape: the offset is unverified, so a foreign number
            // there is a doubt, not a refusal.
            let mut odd = words(&[CA; 12]);
            odd.extend_from_slice(&99i32.to_le_bytes());
            assert_eq!(
                judge_a(&t, 0, &req(ioc(0x46, nr), &odd, &[])),
                Verdict::Doubt(Cause::HandleRef)
            );
        }
    }

    fn export_to_fd(c: u32, fd: i32) -> Vec<u8> {
        let mut n = words(&[0; 6]);
        n[16..20].copy_from_slice(&fd.to_le_bytes());
        ctl(c, c, 0x3D05, &n)
    }

    fn import_from_fd(c: u32, fd: i32) -> Vec<u8> {
        let mut n = words(&[0; 5]);
        n[0..4].copy_from_slice(&fd.to_le_bytes());
        ctl(c, c, 0x3D06, &n)
    }

    #[test]
    fn export_and_import_object_fds_must_be_the_callers_files() {
        let t = table();
        assert_eq!(judge_a(&t, CTL, &export_to_fd(CA, 7)), Verdict::Allow);
        assert_eq!(judge_a(&t, CTL, &import_from_fd(CA, 7)), Verdict::Allow);
        assert_eq!(judge_a(&t, CTL, &export_to_fd(CA, -1)), Verdict::Allow);
        assert_eq!(
            judge_a(&t, CTL, &export_to_fd(CA, 99)),
            Verdict::Deny(Cause::HandleRef)
        );
        assert_eq!(
            judge_a(&t, CTL, &import_from_fd(CA, 99)),
            Verdict::Deny(Cause::HandleRef)
        );
        // The fd-passing attack: B's exported fd handed to A.
        assert_eq!(
            judge_a(&t, CTL, &import_from_fd(CA, 2)),
            Verdict::Deny(Cause::HandleRef)
        );
        // A different block size is not the verified layout.
        let mut odd = words(&[0; 6]);
        odd[0..4].copy_from_slice(&99i32.to_le_bytes());
        assert_eq!(
            judge_a(&t, CTL, &ctl(CA, CA, 0x3D06, &odd)),
            Verdict::Doubt(Cause::HandleRef)
        );
    }

    #[test]
    fn export_family_controls_the_host_does_not_translate_are_counted_not_refused() {
        let t = table();
        for (cmd, off) in [(0x3D08u32, 0usize), (0x3D0A, 72), (0x3D0B, 0), (0x3D0C, 0)] {
            let mut n = vec![0u8; 80];
            n[off..off + 4].copy_from_slice(&99i32.to_le_bytes());
            assert_eq!(
                judge_a(&t, CTL, &ctl(CA, CA, cmd, &n)),
                Verdict::Doubt(Cause::HandleRef),
                "cmd {cmd:#x}"
            );
        }
    }

    // ---- NV0005 / NV0080 / debugger / profiler allocations ----

    fn event_alloc(class: u32, parent_client: u32, data: u64, len: usize) -> Vec<u8> {
        let mut n = words(&[parent_client, 0x55, 0x79, 0]);
        n.extend_from_slice(&data.to_le_bytes());
        n.resize(len, 0);
        alloc64(CA, CA, 0x77, class, &n)
    }

    #[test]
    fn event_class_allocations_check_the_parent_client_and_the_event_file() {
        let t = table();
        for class in [0x05u32, 0x79] {
            assert_eq!(
                judge_a(&t, CTL, &event_alloc(class, CA, 7, 24)),
                Verdict::Allow
            );
            assert_eq!(
                judge_a(&t, CTL, &event_alloc(class, 0, 7, 24)),
                Verdict::Allow,
                "no parent client"
            );
            assert_eq!(
                judge_a(&t, CTL, &event_alloc(class, CB, 7, 24)),
                Verdict::Deny(Cause::ClientRef),
                "events of another process's objects"
            );
            assert_eq!(
                judge_a(&t, CTL, &event_alloc(class, CA, u64::MAX, 24)),
                Verdict::Allow,
                "a negative descriptor names no file"
            );
            // A different size is not the layout the slots were verified in.
            assert_eq!(
                judge_a(&t, CTL, &event_alloc(class, CB, 7, 32)),
                Verdict::Doubt(Cause::ClientRef)
            );
        }
        // Class 0x79 (`NV01_EVENT_OS_EVENT`): `data` is a descriptor, another process's file
        // is refused.
        assert_eq!(
            judge_a(&t, CTL, &event_alloc(0x79, CA, 2, 24)),
            Verdict::Deny(Cause::HandleRef),
            "another process's file"
        );
        // Class 0x05 (`NV01_EVENT`): `data` may be a cookie RM does not read as a descriptor, so
        // a number that is not the caller's handle is a doubt, not a refusal; the parent client
        // is still checked.
        assert_eq!(
            judge_a(&t, CTL, &event_alloc(0x05, CA, 2, 24)),
            Verdict::Doubt(Cause::HandleRef)
        );
        assert_eq!(
            judge_a(&t, CTL, &event_alloc(0x05, CA, 0x7FFF_0000_1234, 24)),
            Verdict::Doubt(Cause::HandleRef),
            "a cookie whose low word is no handle of the caller's"
        );
        assert_eq!(
            judge_a(&t, CTL, &event_alloc(0x05, CB, 0x7FFF_0000_1234, 24)),
            Verdict::Deny(Cause::ClientRef),
            "a refusal outranks a doubt"
        );
        // No parameter block at all: nothing named.
        assert_eq!(
            judge_a(&t, CTL, &alloc64(CA, CA, 9, 0x79, &[])),
            Verdict::Allow
        );
    }

    #[test]
    fn device_alloc_checks_the_share_and_target_clients() {
        let t = table();
        let dev = |share: u32, target: u32, len: usize| {
            let mut n = words(&[0, share, target, 0, 0]);
            n.resize(len, 0);
            alloc64(CA, CA, 0x88, 0x80, &n)
        };
        assert_eq!(judge_a(&t, CTL, &dev(0, 0, 56)), Verdict::Allow);
        assert_eq!(judge_a(&t, CTL, &dev(CA, 0, 56)), Verdict::Allow);
        assert_eq!(
            judge_a(&t, CTL, &dev(CB, 0, 56)),
            Verdict::Deny(Cause::ClientRef)
        );
        assert_eq!(
            judge_a(&t, CTL, &dev(0, CB, 56)),
            Verdict::Deny(Cause::ClientRef)
        );
        assert_eq!(
            judge_a(&t, CTL, &dev(0, CB, 60)),
            Verdict::Doubt(Cause::ClientRef)
        );
    }

    #[test]
    fn debugger_and_profiler_allocations_name_clients() {
        let t = table();
        let dbg = |app: u32| alloc64(CA, CA, 0x99, 0x83DE, &words(&[0, app, 0]));
        assert_eq!(judge_a(&t, CTL, &dbg(CA)), Verdict::Allow);
        assert_eq!(judge_a(&t, CTL, &dbg(CB)), Verdict::Deny(Cause::ClientRef));
        let prof = |c: u32| alloc64(CA, CA, 0x99, 0xB2CC, &words(&[c, 0]));
        assert_eq!(judge_a(&t, CTL, &prof(0)), Verdict::Allow);
        assert_eq!(judge_a(&t, CTL, &prof(CA)), Verdict::Allow);
        assert_eq!(judge_a(&t, CTL, &prof(CB)), Verdict::Deny(Cause::ClientRef));
    }

    #[test]
    fn a_class_alloc_in_someone_elses_client_is_refused_before_its_slots() {
        let t = table();
        let r = alloc64(CB, CB, 0x88, 0x80, &words(&[0; 14]));
        assert_eq!(judge_a(&t, CTL, &r), Verdict::Deny(Cause::CallerClient));
        // An ordinary class with no slots in its parameters.
        assert_eq!(
            judge_a(&t, CTL, &alloc64(CA, CA, 0x88, 0x2080, &words(&[0]))),
            Verdict::Allow
        );
        assert_eq!(
            judge_a(&t, CTL, &alloc64(CB, CB, 0x88, 0x2080, &words(&[0]))),
            Verdict::Deny(Cause::CallerClient)
        );
    }

    // ---- controls with a client in their parameters ----

    fn control_with(cmd: u32, size: usize, off: usize, v: u32) -> Vec<u8> {
        let mut n = vec![0u8; size];
        if off + 4 <= size {
            n[off..off + 4].copy_from_slice(&v.to_le_bytes());
        }
        ctl(CA, 4, cmd, &n)
    }

    #[test]
    fn controls_with_a_client_slot_refuse_another_owners_client() {
        let t = table();
        let cases: [(u32, usize, usize); 11] = [
            (0x0000_0D03, 12, 4),
            (0x2080_2502, 16, 0),
            (0x208F_0403, 16, 4),
            (0x503C_0106, 4, 0),
            (0xA084_0105, 16, 0),
            (0x2080_0122, 48, 0),
            (0x2080_1209, 40, 0),
            (0x2080_1211, 104, 4),
            (0x2080_1211, 112, 4),
            (0x2080_1208, 24, 0),
            (0x2080_1205, 16, 4),
        ];
        for (cmd, size, off) in cases {
            assert_eq!(
                judge_a(&t, CTL, &control_with(cmd, size, off, CA)),
                Verdict::Allow,
                "own client in {cmd:#x}"
            );
            assert_eq!(
                judge_a(&t, CTL, &control_with(cmd, size, off, 0)),
                Verdict::Allow,
                "no client in {cmd:#x}"
            );
            assert_eq!(
                judge_a(&t, CTL, &control_with(cmd, size, off, CB)),
                Verdict::Deny(Cause::ClientRef),
                "foreign client in {cmd:#x}"
            );
        }
    }

    #[test]
    fn a_control_whose_size_moved_is_a_doubt() {
        let t = table();
        // The allow-list's size is 16; 20 is a release this table was not verified in.
        assert_eq!(
            judge_a(&t, CTL, &control_with(0x2080_2502, 20, 0, CB)),
            Verdict::Doubt(Cause::ClientRef)
        );
        // Too short to hold the slot.
        assert_eq!(
            judge_a(&t, CTL, &control_with(0x208F_0403, 4, 4, CB)),
            Verdict::Doubt(Cause::Malformed)
        );
        // No parameters at all: nothing named.
        assert_eq!(
            judge_a(&t, CTL, &ctl(CA, 4, 0x2080_2502, &[])),
            Verdict::Allow
        );
    }

    #[test]
    fn disable_channels_checks_every_listed_client() {
        let t = table();
        let mk = |count: u32, list: &[u32]| {
            let mut n = vec![0u8; 536];
            n[4..8].copy_from_slice(&count.to_le_bytes());
            for (i, c) in list.iter().enumerate() {
                n[24 + 4 * i..28 + 4 * i].copy_from_slice(&c.to_le_bytes());
            }
            ctl(CA, 4, 0x2080_110B, &n)
        };
        assert_eq!(judge_a(&t, CTL, &mk(2, &[CA, CA])), Verdict::Allow);
        assert_eq!(
            judge_a(&t, CTL, &mk(0, &[CB])),
            Verdict::Allow,
            "past the count"
        );
        assert_eq!(
            judge_a(&t, CTL, &mk(2, &[CA, CB])),
            Verdict::Deny(Cause::ClientRef)
        );
        // A count past the array is capped at the array.
        assert_eq!(judge_a(&t, CTL, &mk(u32::MAX, &[CA; 64])), Verdict::Allow);
    }

    #[test]
    fn controls_with_no_slot_only_get_the_callers_client_check() {
        let t = table();
        assert_eq!(
            judge_a(&t, CTL, &ctl(CA, 4, 0x2080_0101, &words(&[CB, CB, CB]))),
            Verdict::Allow,
            "a payload that happens to hold B's number in an unknown control"
        );
    }

    // ---- DRM ----

    #[test]
    fn nvkms_mem_fds_must_be_the_callers_files() {
        let t = table();
        for nr in [0x41u32, 0x49, 0x4D] {
            let mk = |fd: i32| req(ioc(0x64, nr), &[0; 32], &words(&[fd as u32, 5, 0, 0]));
            assert_eq!(judge_a(&t, DRM, &mk(7)), Verdict::Allow, "nr {nr:#x}");
            assert_eq!(
                judge_a(&t, DRM, &mk(99)),
                Verdict::Deny(Cause::HandleRef),
                "nr {nr:#x}"
            );
            assert_eq!(judge_a(&t, DRM, &mk(-1)), Verdict::Allow);
            // No nested block at all: the host refuses it; counted here.
            assert_eq!(
                judge_a(&t, DRM, &req(ioc(0x64, nr), &[0; 32], &[])),
                Verdict::Doubt(Cause::Malformed)
            );
        }
    }

    #[test]
    fn fence_ctx_create_names_a_client_and_fence_wait_counts_foreign_fences() {
        let t = table();
        let mk = |c: u32| req(ioc(0x64, 0x54), &[0; 32], &words(&[c, 3, 0, 0]));
        assert_eq!(judge_a(&t, DRM, &mk(CA)), Verdict::Allow);
        assert_eq!(judge_a(&t, DRM, &mk(CB)), Verdict::Deny(Cause::ClientRef));
        let wait = |fd: i32| req(ioc(0x64, 0x56), &words(&[1, fd as u32, 0, 0, 0, 0]), &[]);
        assert_eq!(judge_a(&t, DRM, &wait(7)), Verdict::Allow);
        assert_eq!(
            judge_a(&t, DRM, &wait(0)),
            Verdict::Allow,
            "already signalled"
        );
        assert_eq!(judge_a(&t, DRM, &wait(-1)), Verdict::Allow);
        assert_eq!(
            judge_a(&t, DRM, &wait(99)),
            Verdict::Doubt(Cause::HandleRef),
            "a fence the caller did not create is counted, never refused"
        );
        // Fence create returns a handle; it carries none.
        let create = req(ioc(0x64, 0x55), &words(&[1, 0, 0, 0, 99, 0]), &[]);
        assert_eq!(judge_a(&t, DRM, &create), Verdict::Allow);
    }

    // ---- hostile / truncated input, at scale ----

    #[test]
    fn no_prefix_of_any_request_shape_panics_and_denials_need_their_whole_slot() {
        let t = table();
        let shapes: Vec<Vec<u8>> = vec![
            ctl(CB, 4, 0x2080_110B, &vec![0xFF; 536]),
            ctl(CB, 4, 0x3D06, &words(&[99, 0, 0, 0, 0])),
            alloc64(CB, CB, 1, 0x79, &words(&[CB, 0, 0, 0, 99, 0])),
            alloc21(0, 0, 0, 0x41, &[]),
            dup(CB, CB, 28),
            req(ioc(0x64, 0x41), &[0; 32], &words(&[99, 0, 0, 0])),
            req(
                ioc(0x46, NV_ESC_ALLOC_OS_EVENT),
                &words(&[CB, 0, 99, 0]),
                &[],
            ),
        ];
        for s in &shapes {
            for cut in 0..=s.len() {
                let _ = judge_a(&t, CTL, &s[..cut]);
                let _ = root_alloc(&s[..cut]);
                let _ = client_free(&s[..cut]);
                let _ = client_from_reply(&s[..cut], s, s.len());
            }
        }
    }

    #[test]
    fn pseudo_random_requests_never_panic() {
        let t = table();
        let mut x = 0x2545_F491_4F6C_DD1Du64;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let nrs = [
            0x27u32, 0x29, 0x2A, 0x2B, 0x34, 0x4E, 0x54, 0x56, 0x41, 0x49, 0xC9, 0xCE, 0x5E,
        ];
        for i in 0..20_000 {
            let r = next();
            let ty = if r & 1 == 0 { 0x46 } else { 0x64 };
            let nr = nrs[(r >> 8) as usize % nrs.len()];
            let dlen = ((r >> 16) % 64) as usize;
            let nlen = ((r >> 24) % 600) as usize;
            let mut data = vec![0u8; dlen];
            let mut nested = vec![0u8; nlen];
            for b in data.iter_mut().chain(nested.iter_mut()) {
                *b = (next() >> 11) as u8;
            }
            // Bias some words towards live numbers so deep paths run.
            if dlen >= 4 && i % 3 == 0 {
                data[0..4].copy_from_slice(&CA.to_le_bytes());
            }
            if dlen >= 12 && i % 5 == 0 {
                data[8..12].copy_from_slice(&0x3D05u32.to_le_bytes());
            }
            let mut q = req(ioc(ty, nr), &data, &nested);
            if i % 7 == 0 {
                // Lie about the lengths.
                q[20..24].copy_from_slice(&(next() as u32).to_le_bytes());
            }
            if i % 11 == 0 {
                q[28..32].copy_from_slice(&(next() as u32).to_le_bytes());
            }
            let _ = judge_a(&t, CTL, &q);
            let _ = root_alloc(&q);
            let _ = client_free(&q);
            let _ = client_from_reply(&q, &q, q.len());
            let _ = free_reply_ok(&q, q.len());
        }
    }

    #[test]
    fn a_single_client_session_is_untouched() {
        // One process, one client, its own files: the shape of NVK / DXVK / vkd3d.
        let mut t = ClientTable::new();
        assert!(t.reserve(A));
        assert_eq!(t.commit(A, 1, CA, true), Commit::Recorded);
        let session: Vec<Vec<u8>> = vec![
            ctl(CA, CA, 0x0000_0102, &[0; 8]),
            ctl(CA, 4, 0x2080_0122, &vec![0; 48]),
            alloc64(
                CA,
                CA,
                0x10,
                0x80,
                &words(&[0, CA, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
            ),
            alloc64(CA, 0x10, 0x11, 0x2080, &words(&[0])),
            alloc64(CA, 0x11, 0x12, 0x79, &event_nested(CA, 7)),
            req(ioc(0x46, NV_ESC_REGISTER_FD), &1i32.to_le_bytes(), &[]),
            req(
                ioc(0x46, NV_ESC_ALLOC_OS_EVENT),
                &words(&[CA, 0x10, 7, 0]),
                &[],
            ),
            dup(CA, CA, 28),
            export_to_fd(CA, 7),
            import_from_fd(CA, 7),
            req(ioc(0x64, 0x41), &[0; 32], &words(&[7, 5, 0, 0])),
            req(ioc(0x64, 0x56), &words(&[1, 7, 0, 0, 0, 0]), &[]),
            req(ioc(0x46, 0x29), &words(&[CA, 0, CA, 0]), &[]),
        ];
        for (i, r) in session.iter().enumerate() {
            assert_eq!(judge_a(&t, CTL, r), Verdict::Allow, "request {i}");
        }
    }

    fn event_nested(parent: u32, fd: u32) -> Vec<u8> {
        let mut n = words(&[parent, 0x55, 0x79, 0]);
        n.extend_from_slice(&(fd as u64).to_le_bytes());
        n
    }

    /// The forward path as `virtio/nvrm_harden.rs` drives it, over the pure pieces: judge,
    /// reserve, send (the reply is the caller's), then record or forget from the reply.
    /// Returns what the KMD would answer: `Err` = refused before the host saw it.
    fn forward(
        t: &mut ClientTable,
        owner: usize,
        via: u32,
        req: &[u8],
        host_reply: &[u8],
        opened: &[u32],
    ) -> Result<(), Verdict> {
        match judge(t, owner, CTL, req, |h| opened.contains(&h)) {
            Verdict::Deny(c) => return Err(Verdict::Deny(c)),
            _ => {}
        }
        let reserved = root_alloc(req) && t.reserve(owner);
        if root_alloc(req) && !reserved {
            return Err(Verdict::Doubt(Cause::Malformed));
        }
        if root_alloc(req) {
            match client_from_reply(req, host_reply, host_reply.len()) {
                Some(c) => {
                    t.commit(owner, via, c, true);
                }
                None => t.cancel(owner),
            }
        } else if let Some(c) = client_free(req) {
            if free_reply_ok(host_reply, host_reply.len()) {
                t.forget_client(owner, c);
            }
        }
        Ok(())
    }

    #[test]
    fn two_processes_cannot_reach_each_others_clients_through_a_whole_session() {
        let mut t = ClientTable::new();
        let root_req = alloc64(0, 0, 0, 0x41, &[]);
        let root_reply = |c: u32| reply(0, &words(&[0, 0, c, 0x41, 0, 0, 0, 0, 0, 0, 0, 0]));
        // Both processes open a client through their own control file.
        assert!(forward(&mut t, A, 1, &root_req, &root_reply(CA), &[1]).is_ok());
        assert!(forward(&mut t, B, 1, &root_req, &root_reply(CB), &[1]).is_ok());
        assert_eq!(t.len(), 2);

        // Each works in its own client.
        let ok = reply(0, &[]);
        assert!(forward(&mut t, A, 1, &ctl(CA, 4, 0x1234, &[]), &ok, &[1]).is_ok());
        assert!(forward(&mut t, B, 1, &ctl(CB, 4, 0x1234, &[]), &ok, &[1]).is_ok());
        // A reaches into B's client, three ways.
        assert!(forward(&mut t, A, 1, &ctl(CB, 4, 0x1234, &[]), &ok, &[1]).is_err());
        assert!(forward(&mut t, A, 1, &dup(CA, CB, 28), &ok, &[1]).is_err());
        let ev = alloc64(CA, CA, 9, 0x79, &event_nested(CB, 1));
        assert!(forward(&mut t, A, 1, &ev, &ok, &[1]).is_err());
        // B's exported fd, "passed" to A (the number is B's file, not A's).
        assert!(forward(&mut t, A, 1, &import_from_fd(CA, 5), &ok, &[1]).is_err());

        // A frees its client; the number is B's problem no more, and A cannot use it.
        let free_a = req(ioc(0x46, 0x29), &words(&[CA, 0, CA, 0]), &[]);
        let free_ok = reply(0, &words(&[CA, 0, CA, 0]));
        assert!(forward(&mut t, A, 1, &free_a, &free_ok, &[1]).is_ok());
        assert!(!t.is_client_owned_by(A, CA));
        assert!(forward(&mut t, A, 1, &ctl(CA, 4, 0x1234, &[]), &ok, &[1]).is_err());
        // RM mints B the number A just freed: B owns it, A still does not.
        assert!(forward(&mut t, B, 2, &root_req, &root_reply(CA), &[1, 2]).is_ok());
        assert!(t.is_client_owned_by(B, CA));
        assert!(forward(&mut t, A, 1, &ctl(CA, 4, 0x1234, &[]), &ok, &[1]).is_err());

        // B's file 1 closes: only the client made through it goes.
        assert_eq!(t.forget_via(B, 1), 1);
        assert!(!t.is_client_owned_by(B, CB));
        assert!(t.is_client_owned_by(B, CA));
        // B's device is destroyed: nothing of it is left to name.
        t.forget_owner(B);
        assert!(t.is_empty());
    }

    #[test]
    fn a_refused_client_allocation_leaves_no_reservation_behind() {
        let mut t = ClientTable::new();
        let root_req = alloc21(0, 0, 0, 0x41, &[]);
        // RM said no.
        let no = reply(0, &words(&[0, 0, 0, 0x41, 0, 0, 0, 0x1F]));
        for _ in 0..(MAX_CLIENTS_PER_OWNER * 3) {
            assert!(forward(&mut t, A, 1, &root_req, &no, &[1]).is_ok());
        }
        assert!(t.is_empty());
        // The quota is still whole.
        for i in 0..MAX_CLIENTS_PER_OWNER {
            let yes = reply(0, &words(&[0, 0, 0x100 + i as u32, 0x41, 0, 0, 0, 0]));
            assert!(forward(&mut t, A, 1, &root_req, &yes, &[1]).is_ok(), "{i}");
        }
        assert_eq!(t.count_for(A), MAX_CLIENTS_PER_OWNER);
    }
}
