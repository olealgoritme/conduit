//! Guest-memory blobs (docs/VENUS.md "Guest-memory blobs") against the Mock
//! and fake guest RAM.

use super::*;
use crate::guestmem::fake::FakeRam;
use crate::venus::guest::Live;
use conduit_venus::PageRun;

const P: u64 = 4096;

/// Two regions of one memfd, with a hole between them as a real guest has:
/// guest [0, 64 pages) at file offset 0, guest [1 GiB, +64 pages) right
/// after it in the file.
fn qemu_ram() -> FakeRam {
    FakeRam::one_file(&[(0, 64 * P), (1 << 30, 64 * P)])
}

fn guest_rig() -> Rig {
    let mut t = Rig::new();
    t.ram = Some(qemu_ram());
    assert!(t.venus.enable_guest_blobs(), "the mock imports guest pages");
    t.ctx(1);
    t
}

fn guest_blob(ctx: u32, id: u32, entries: &[(u64, u32)], size: u64) -> Vec<u8> {
    let c = ResourceCreateBlob {
        hdr: hdr(CMD_RESOURCE_CREATE_BLOB, ctx),
        resource_id: id,
        blob_mem: BLOB_MEM_GUEST,
        blob_flags: BLOB_FLAG_USE_SHAREABLE,
        nr_entries: entries.len() as u32,
        blob_id: 0,
        size,
    };
    let mut b = c.to_bytes().to_vec();
    for &(addr, len) in entries {
        b.extend_from_slice(&addr.to_le_bytes());
        b.extend_from_slice(&len.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes());
    }
    b
}

/// The error type and the errno echoed in its header.
fn refusal(t: &mut Rig, cmd: &[u8]) -> (u32, i32) {
    let h = t.send(cmd).0;
    let p = h.padding;
    (h.ty, i32::from_le_bytes([p[0], p[1], p[2], 0]))
}

fn set_flags(cmd: &mut [u8], flags: u32) {
    cmd[32..36].copy_from_slice(&flags.to_le_bytes());
}

#[test]
fn guest_blobs_are_refused_unless_enabled() {
    let mut t = Rig::new();
    t.ram = Some(qemu_ram());
    t.ctx(1);
    assert!(!t.venus.guest_blobs());
    let cmd = guest_blob(1, 10, &[(0, P as u32)], P);
    assert_eq!(t.ty(&cmd), RESP_ERR_INVALID_PARAMETER);
    assert!(t.r.mock.lock().unwrap().guest.is_empty());
}

#[test]
fn a_guest_blob_is_the_guest_pages_in_order() {
    let mut t = guest_rig();
    // Pages 5-6, then 1 GiB + page 3 (file page 67), then page 7: adjacent to
    // 5-6 in the guest but not in the list, so it is a run of its own.
    let entries = [
        (5 * P, 2 * P as u32),
        ((1 << 30) + 3 * P, P as u32),
        (7 * P, P as u32),
    ];
    assert_eq!(t.ty(&guest_blob(1, 10, &entries, 4 * P)), RESP_OK_NODATA);
    {
        let m = t.r.mock.lock().unwrap();
        assert_eq!(
            m.guest[&10],
            vec![
                PageRun {
                    offset: 5 * P,
                    len: 2 * P
                },
                PageRun {
                    offset: 67 * P,
                    len: P
                },
                PageRun {
                    offset: 7 * P,
                    len: P
                },
            ]
        );
        assert_eq!(m.resources.get(&10), Some(&(4 * P)));
    }
    assert_eq!(t.venus.resources[&10].attached, HashSet::from([1]));
    assert_eq!(
        t.venus.guest_live(),
        Live {
            blobs: 1,
            runs: 3,
            bytes: 4 * P
        }
    );
    // The KMD's CTX_ATTACH_RESOURCE after the create is a no-op.
    assert_eq!(
        t.ty(&res_cmd(CMD_CTX_ATTACH_RESOURCE, 1, 10)),
        RESP_OK_NODATA
    );
    // Not mappable: the guest has the pages.
    let map = ResourceMapBlob {
        hdr: hdr(CMD_RESOURCE_MAP_BLOB, 1),
        resource_id: 10,
        padding: 0,
        offset: 0,
    };
    assert_eq!(t.ty(&map.to_bytes()), RESP_ERR_INVALID_PARAMETER);
    // Unref gives everything back.
    assert_eq!(t.ty(&res_cmd(CMD_RESOURCE_UNREF, 1, 10)), RESP_OK_NODATA);
    assert!(t.r.mock.lock().unwrap().guest.is_empty());
    assert_eq!(t.venus.guest_live(), Live::default());
}

#[test]
fn runs_adjacent_in_guest_ram_are_merged() {
    let mut t = guest_rig();
    let entries: Vec<(u64, u32)> = (0..8).map(|i| (i * P, P as u32)).collect();
    assert_eq!(t.ty(&guest_blob(1, 10, &entries, 8 * P)), RESP_OK_NODATA);
    assert_eq!(
        t.r.mock.lock().unwrap().guest[&10],
        vec![PageRun {
            offset: 0,
            len: 8 * P
        }]
    );
    assert_eq!(t.venus.guest_live().runs, 1);
}

#[test]
fn a_bad_shape_is_refused_with_einval() {
    let mut t = guest_rig();
    let one = [(0, P as u32)];
    let mut mappable = guest_blob(1, 10, &one, P);
    set_flags(&mut mappable, BLOB_FLAG_USE_MAPPABLE);
    let mut cross = guest_blob(1, 10, &one, P);
    set_flags(&mut cross, BLOB_FLAG_USE_CROSS_DEVICE);
    let mut blob_id = guest_blob(1, 10, &one, P);
    blob_id[40..48].copy_from_slice(&1u64.to_le_bytes());
    let too_many: Vec<(u64, u32)> = (0..=GUEST_BLOB_MAX_ENTRIES as u64)
        .map(|_| (0, P as u32))
        .collect();
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("mappable", mappable),
        ("cross-device", cross),
        ("blob_id", blob_id),
        ("no entries", guest_blob(1, 10, &[], P)),
        (
            "size short of the entries",
            guest_blob(1, 10, &[(0, 2 * P as u32)], P),
        ),
        ("size past the entries", guest_blob(1, 10, &one, 2 * P)),
        ("size not pages", guest_blob(1, 10, &[(0, 100)], 100)),
        (
            "address not a page",
            guest_blob(1, 10, &[(100, P as u32)], P),
        ),
        (
            "length not pages",
            guest_blob(1, 10, &[(0, P as u32), (P, 100)], P + 100),
        ),
        (
            "empty entry",
            guest_blob(1, 10, &[(0, 0), (0, P as u32)], P),
        ),
        (
            "too many entries",
            guest_blob(1, 10, &too_many, (GUEST_BLOB_MAX_ENTRIES as u64 + 1) * P),
        ),
        (
            "too big",
            guest_blob(
                1,
                10,
                &[(0, (GUEST_BLOB_MAX_BYTES / 2) as u32); 3],
                GUEST_BLOB_MAX_BYTES / 2 * 3,
            ),
        ),
    ];
    for (what, cmd) in cases {
        assert_eq!(
            refusal(&mut t, &cmd),
            (RESP_ERR_INVALID_PARAMETER, libc::EINVAL),
            "{what}"
        );
    }
    assert!(t.r.mock.lock().unwrap().guest.is_empty());
    assert_eq!(t.venus.guest_live(), Live::default());
    // Ids and contexts as for any blob.
    assert_eq!(
        t.ty(&guest_blob(9, 10, &one, P)),
        RESP_ERR_INVALID_CONTEXT_ID
    );
    assert_eq!(
        t.ty(&guest_blob(1, 0, &one, P)),
        RESP_ERR_INVALID_RESOURCE_ID
    );
    assert_eq!(t.blob(1, 10, 4096), RESP_OK_NODATA);
    assert_eq!(
        t.ty(&guest_blob(1, 10, &one, P)),
        RESP_ERR_INVALID_RESOURCE_ID
    );
}

#[test]
fn pages_outside_guest_ram_are_refused_with_efault() {
    let mut t = guest_rig();
    // In the hole between the regions.
    let hole = guest_blob(1, 10, &[(64 * P, P as u32)], P);
    assert_eq!(
        refusal(&mut t, &hole),
        (RESP_ERR_INVALID_PARAMETER, libc::EFAULT)
    );
    // Running off the end of the first region into the hole (the file goes
    // on into the second region there, which must not be reached).
    let cross = guest_blob(1, 10, &[(63 * P, 2 * P as u32)], 2 * P);
    assert_eq!(
        refusal(&mut t, &cross),
        (RESP_ERR_INVALID_PARAMETER, libc::EFAULT)
    );
    assert!(t.r.mock.lock().unwrap().guest.is_empty());
}

#[test]
fn pages_from_two_ram_files_are_refused_with_exdev() {
    let mut t = guest_rig();
    // Separate memfds per region, as a VM with two memory backends has.
    t.ram = Some(FakeRam::new(&[(0, 64 * P), (1 << 30, 64 * P)]));
    let cmd = guest_blob(1, 10, &[(0, P as u32), (1 << 30, P as u32)], 2 * P);
    assert_eq!(
        refusal(&mut t, &cmd),
        (RESP_ERR_INVALID_PARAMETER, libc::EXDEV)
    );
    // Either region alone is fine.
    assert_eq!(
        t.ty(&guest_blob(1, 10, &[(1 << 30, P as u32)], P)),
        RESP_OK_NODATA
    );
}

#[test]
fn without_guest_ram_a_guest_blob_is_unsupported() {
    let mut t = guest_rig();
    t.ram = None;
    let cmd = guest_blob(1, 10, &[(0, P as u32)], P);
    assert_eq!(refusal(&mut t, &cmd), (RESP_ERR_UNSPEC, libc::EOPNOTSUPP));
}

#[test]
fn the_live_limits_are_held() {
    let mut t = guest_rig();
    t.venus.guest_live = Live {
        blobs: GUEST_BLOB_MAX_LIVE,
        runs: 0,
        bytes: 0,
    };
    let cmd = guest_blob(1, 10, &[(0, P as u32)], P);
    assert_eq!(
        refusal(&mut t, &cmd),
        (RESP_ERR_OUT_OF_MEMORY, libc::ENOMEM)
    );
    t.venus.guest_live = Live {
        blobs: 0,
        runs: GUEST_BLOB_MAX_LIVE_RUNS,
        bytes: 0,
    };
    assert_eq!(
        refusal(&mut t, &cmd),
        (RESP_ERR_OUT_OF_MEMORY, libc::ENOMEM)
    );
    t.venus.guest_live = Live {
        blobs: 0,
        runs: 0,
        bytes: GUEST_BLOB_MAX_LIVE_BYTES,
    };
    assert_eq!(
        refusal(&mut t, &cmd),
        (RESP_ERR_OUT_OF_MEMORY, libc::ENOMEM)
    );
    t.venus.guest_live = Live::default();
    assert_eq!(t.ty(&cmd), RESP_OK_NODATA);
}

#[test]
fn a_renderer_refusal_leaves_nothing() {
    let mut t = guest_rig();
    // The renderer already has a resource 10 the backend does not know of.
    t.r.mock.lock().unwrap().resources.insert(10, P);
    let cmd = guest_blob(1, 10, &[(0, P as u32)], P);
    assert_eq!(refusal(&mut t, &cmd), (RESP_ERR_UNSPEC, libc::EIO));
    assert!(!t.venus.resources.contains_key(&10));
    assert_eq!(t.venus.guest_live(), Live::default());
}

#[test]
fn a_reset_forgets_every_guest_blob() {
    let mut t = guest_rig();
    assert_eq!(
        t.ty(&guest_blob(1, 10, &[(0, P as u32)], P)),
        RESP_OK_NODATA
    );
    assert_eq!(
        t.ty(&guest_blob(1, 11, &[(P, P as u32)], P)),
        RESP_OK_NODATA
    );
    assert_eq!(t.venus.guest_live().blobs, 2);
    t.venus.reset(None);
    assert_eq!(t.venus.guest_live(), Live::default());
    assert!(t.r.mock.lock().unwrap().guest.is_empty());
}
