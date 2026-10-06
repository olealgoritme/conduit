//! RM-export blobs (docs/VENUS.md "RM-export blobs") against the Mock and a
//! fake RM side.

use super::*;

const BL_H5: u64 = 0x0300_0000_0060_6015;

fn rm_blob_cmd(ctx: u32, id: u32, rm: u32, gem: u32, size: u64) -> ResourceCreateBlob {
    ResourceCreateBlob {
        hdr: hdr(CMD_RESOURCE_CREATE_BLOB, ctx),
        resource_id: id,
        blob_mem: BLOB_MEM_RM_EXPORT,
        blob_flags: 0,
        nr_entries: 0,
        blob_id: (u64::from(rm) << 32) | u64::from(gem),
        size,
    }
}

/// The error type and the errno echoed in its header.
fn refusal(t: &mut Rig, c: &ResourceCreateBlob) -> (u32, i32) {
    let h = t.send(&c.to_bytes()).0;
    let p = h.padding;
    (h.ty, i32::from_le_bytes([p[0], p[1], p[2], 0]))
}

#[test]
fn an_rm_blob_is_imported_attached_and_held() {
    let mut t = Rig::new();
    assert!(t.venus.rm_import(), "the mock renderer imports dma-bufs");
    // 1920x1080, block-linear h = 5: 1280 rows of 7680 bytes, rounded by RM
    // to 64 KiB.
    let object = (7680u64 * 1280).next_multiple_of(64 << 10);
    t.rm.objects.insert((7, 77), (object, Some(BL_H5)));
    t.ctx(1);
    t.ctx(2);
    let size = 7680 * 1280;
    assert_eq!(
        t.ty(&rm_blob_cmd(1, 50, 7, 77, size).to_bytes()),
        RESP_OK_NODATA
    );
    assert_eq!(t.venus.rm_modifier(50), Some(Some(BL_H5)));

    // The renderer has the very object, as resource 50, at the guest's size.
    let exported = t.rm.exported.lock().unwrap().clone();
    assert_eq!(exported.len(), 1);
    {
        let m = t.r.mock.lock().unwrap();
        assert_eq!(m.resources.get(&50), Some(&size));
        assert_eq!(inode(m.imported[&50].as_fd()), exported[0]);
    }
    // And so does the backend: its own reference, not borrowed from the
    // RM side.
    assert_eq!(inode(t.venus.resources[&50].fd.as_fd()), exported[0]);
    assert_eq!(t.venus.resources[&50].attached, HashSet::from([1]));

    // The KMD's CTX_ATTACH_RESOURCE that follows the create is a no-op; a
    // second context (the D3D bridge's) attaches as to any resource.
    assert_eq!(
        t.ty(&res_cmd(CMD_CTX_ATTACH_RESOURCE, 1, 50)),
        RESP_OK_NODATA
    );
    assert_eq!(
        t.ty(&res_cmd(CMD_CTX_ATTACH_RESOURCE, 2, 50)),
        RESP_OK_NODATA
    );
    // No CPU view of RM memory.
    assert_eq!(t.map(50, 0), RESP_ERR_INVALID_PARAMETER);

    // The guest closes the render node (the object is gone from the RM
    // side): the resource does not notice.
    t.rm.objects.clear();
    assert_eq!(t.venus.resources(), 1);
    assert_eq!(
        t.ty(&res_cmd(CMD_CTX_DETACH_RESOURCE, 2, 50)),
        RESP_OK_NODATA
    );

    // UNREF lets go of both references.
    assert_eq!(t.ty(&res_cmd(CMD_RESOURCE_UNREF, 0, 50)), RESP_OK_NODATA);
    assert_eq!(t.venus.resources(), 0);
    let m = t.r.mock.lock().unwrap();
    assert!(m.imported.is_empty() && !m.resources.contains_key(&50));
}

#[test]
fn an_rm_blob_may_be_smaller_than_its_object_but_not_larger() {
    let mut t = Rig::new();
    t.rm.objects.insert((7, 77), (1 << 20, Some(0)));
    t.ctx(1);
    assert_eq!(
        refusal(&mut t, &rm_blob_cmd(1, 50, 7, 77, (1 << 20) + 4096)),
        (RESP_ERR_INVALID_PARAMETER, libc::ERANGE)
    );
    assert_eq!(t.venus.resources(), 0);
    assert!(
        t.r.mock.lock().unwrap().imported.is_empty(),
        "refused before the renderer"
    );
    assert_eq!(
        t.ty(&rm_blob_cmd(1, 50, 7, 77, 1 << 20).to_bytes()),
        RESP_OK_NODATA
    );
    assert_eq!(
        t.ty(&rm_blob_cmd(1, 51, 7, 77, 4096).to_bytes()),
        RESP_OK_NODATA
    );
}

#[test]
fn rm_blob_refusals_say_why() {
    let mut t = Rig::new();
    t.rm.objects.insert((7, 77), (1 << 20, None));
    t.ctx(1);
    let ok = rm_blob_cmd(1, 50, 7, 77, 1 << 20);
    // Not a render node the guest opened, and a GEM handle that file lacks.
    assert_eq!(
        refusal(&mut t, &rm_blob_cmd(1, 50, 9, 77, 1 << 20)),
        (RESP_ERR_INVALID_PARAMETER, libc::EBADF)
    );
    assert_eq!(
        refusal(&mut t, &rm_blob_cmd(1, 50, 7, 78, 1 << 20)),
        (RESP_ERR_INVALID_PARAMETER, libc::ENOENT)
    );
    // Its shape: no flags but MAPPABLE and SHAREABLE, no entries, a size.
    for c in [
        ResourceCreateBlob {
            blob_flags: BLOB_FLAG_USE_CROSS_DEVICE,
            ..ok
        },
        ResourceCreateBlob {
            blob_flags: BLOB_FLAG_USE_MAPPABLE,
            size: HOSTMEM + 4096,
            ..ok
        },
        ResourceCreateBlob { size: 0, ..ok },
    ] {
        assert_eq!(
            refusal(&mut t, &c),
            (RESP_ERR_INVALID_PARAMETER, libc::EINVAL)
        );
    }
    // MAPPABLE of memory whose placement the backend did not follow.
    assert_eq!(
        refusal(
            &mut t,
            &ResourceCreateBlob {
                blob_flags: BLOB_FLAG_USE_MAPPABLE,
                ..ok
            }
        ),
        (RESP_ERR_INVALID_PARAMETER, libc::EOPNOTSUPP)
    );
    // Context and resource ids as for any blob; no errno for those.
    assert_eq!(
        refusal(&mut t, &rm_blob_cmd(3, 50, 7, 77, 1 << 20)),
        (RESP_ERR_INVALID_CONTEXT_ID, 0)
    );
    assert_eq!(
        refusal(&mut t, &rm_blob_cmd(1, 0, 7, 77, 1 << 20)),
        (RESP_ERR_INVALID_RESOURCE_ID, 0)
    );
    assert_eq!(t.ty(&ok.to_bytes()), RESP_OK_NODATA);
    assert_eq!(
        refusal(&mut t, &ok),
        (RESP_ERR_INVALID_RESOURCE_ID, 0),
        "taken"
    );
    // An errno is never left over for the next command.
    assert_eq!(
        refusal(&mut t, &rm_blob_cmd(1, 51, 9, 77, 4096)).1,
        libc::EBADF
    );
    assert_eq!(t.send(&map_cmd(999, 0)).0.padding, [0; 3]);
    assert_eq!(t.venus.resources(), 1);
}

/// A renderer from before `import_dmabuf`: the trait's defaults.
struct Old(Shared);

impl Renderer for Old {
    fn capset_info(&mut self, i: u32) -> conduit_venus::Result<CapsetInfo> {
        self.0.capset_info(i)
    }
    fn capset(&mut self, i: u32, v: u32) -> conduit_venus::Result<Vec<u8>> {
        self.0.capset(i, v)
    }
    fn ctx_create(&mut self, c: u32, s: u32, n: &[u8]) -> conduit_venus::Result<()> {
        self.0.ctx_create(c, s, n)
    }
    fn ctx_destroy(&mut self, c: u32) {
        self.0.ctx_destroy(c)
    }
    fn ctx_attach(&mut self, c: u32, r: u32) -> conduit_venus::Result<()> {
        self.0.ctx_attach(c, r)
    }
    fn ctx_detach(&mut self, c: u32, r: u32) {
        self.0.ctx_detach(c, r)
    }
    fn submit(&mut self, c: u32, s: &[u8]) -> conduit_venus::Result<()> {
        self.0.submit(c, s)
    }
    fn create_blob(
        &mut self,
        c: u32,
        r: u32,
        b: u64,
        s: u64,
        f: u32,
    ) -> conduit_venus::Result<Blob> {
        self.0.create_blob(c, r, b, s, f)
    }
    fn unref(&mut self, r: u32) {
        self.0.unref(r)
    }
    fn create_fence(&mut self, c: u32, r: u32, f: u64) -> conduit_venus::Result<()> {
        self.0.create_fence(c, r, f)
    }
    fn fence_fd(&self) -> BorrowedFd<'_> {
        self.0.fence_fd()
    }
    fn signalled(&mut self) -> conduit_venus::Result<Vec<Signalled>> {
        self.0.signalled()
    }
    fn export_scanout(&mut self, r: u32, l: ScanoutLayout) -> conduit_venus::Result<Dmabuf> {
        self.0.export_scanout(r, l)
    }
}

#[test]
fn rm_blobs_need_a_renderer_that_imports_and_an_rm_side() {
    let mut t = Rig::new();
    t.venus = Venus::new(Box::new(Old(t.r.clone())), HOSTMEM, None);
    assert!(!t.venus.rm_import());
    t.rm.objects.insert((7, 77), (1 << 20, None));
    t.ctx(1);
    assert_eq!(
        refusal(&mut t, &rm_blob_cmd(1, 50, 7, 77, 1 << 20)),
        (RESP_ERR_UNSPEC, libc::EOPNOTSUPP)
    );
    assert!(
        t.rm.exported.lock().unwrap().is_empty(),
        "nothing exported for it"
    );

    // With the feature but no RM side lent.
    let mut t = Rig::new();
    t.ctx(1);
    let mut resp = vec![0u8; 1024];
    let env = Env {
        window: Some(&*t.region),
        display: None,
        rm: None,
        ram: None,
    };
    let cmd = rm_blob_cmd(1, 50, 7, 77, 1 << 20).to_bytes();
    let Outcome::Done(n) = t.venus.dispatch(&cmd, &mut resp, env) else {
        panic!("held")
    };
    let h = CtrlHdr::from_bytes(&resp[16..n]).unwrap();
    assert_eq!(
        (h.ty, h.padding[0] as i32),
        (RESP_ERR_UNSPEC, libc::EOPNOTSUPP)
    );
}

#[test]
fn an_rm_scanout_has_the_layout_nvk_imported_it_with() {
    let mut t = Rig::new();
    let (link, broker) = display();
    // Block-linear h = 5, which the size rule would call h = 4; and a linear
    // object big enough that the size rule would call it block-linear.
    t.rm.objects.insert((7, 77), (7680 * 1280, Some(BL_H5)));
    t.rm.objects.insert((7, 78), (7680 * 1152, Some(0)));
    // Seen by no import: the size rule is all there is.
    t.rm.objects.insert((7, 79), (7680 * 1152, None));
    t.ctx(1);
    for (id, gem) in [(50, 77), (51, 78), (52, 79)] {
        let size = t.rm.objects[&(7, gem)].0;
        assert_eq!(
            t.ty(&rm_blob_cmd(1, id, 7, gem, size).to_bytes()),
            RESP_OK_NODATA
        );
    }
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
    for (res, want) in [(50, BL_H5), (51, 0), (52, 0x0300_0000_0060_6014)] {
        assert_eq!(t.ty(&scanout_cmd(res, 1920, 1080)), RESP_OK_NODATA);
        assert_eq!(t.venus.scanout_modifier(), Some(want), "res {res}");
        assert_eq!(t.send_on(&flush_cmd(res), Some(&link)).0.ty, RESP_OK_NODATA);
        assert_eq!(attached(&broker), vec![want], "res {res}");
    }
    // Shown from the backend's own dma-buf: the renderer exported nothing.
    assert!(t.r.exports.lock().unwrap().is_empty());
    // The override still wins, for experiments.
    t.venus.forced_modifier = Some(0x0300_0000_0060_6010);
    assert_eq!(t.ty(&scanout_cmd(50, 1920, 1080)), RESP_OK_NODATA);
    assert_eq!(t.venus.scanout_modifier(), Some(0x0300_0000_0060_6010));
}

#[test]
fn a_reset_lets_go_of_every_rm_blob() {
    let mut t = Rig::new();
    t.rm.objects.insert((7, 77), (1 << 20, Some(0)));
    t.ctx(1);
    assert_eq!(
        t.ty(&rm_blob_cmd(1, 50, 7, 77, 1 << 20).to_bytes()),
        RESP_OK_NODATA
    );
    // The backend's descriptor and the renderer's both go with a reset.
    let ino = inode(t.venus.resources[&50].fd.as_fd());
    t.venus.reset(None);
    assert_eq!(t.venus.resources(), 0);
    assert!(t.r.mock.lock().unwrap().imported.is_empty());
    // No descriptor in this process still names the object.
    let open = std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().to_str()?.parse::<RawFd>().ok())
        .filter(|&fd| {
            // SAFETY: fstat on a number that may or may not be open.
            let mut st: libc::stat = unsafe { std::mem::zeroed() };
            unsafe { libc::fstat(fd, &mut st) == 0 && st.st_ino == ino }
        })
        .count();
    assert_eq!(open, 0, "the object is still held");
}

/// RM `attr` as RM answers it for cached PCI sysmem, WC PCI sysmem and video
/// memory (measured with rm_sysmem_flip on the host).
const SYS_CACHED: u32 = 0x2a80_0000;
const SYS_WC: u32 = 0x4a80_0000;
const VIDMEM: u32 = 0x1100_0000;

/// A MAPPABLE RM-export blob of system memory is mapped like a HOST3D blob:
/// its dma-buf placed at the guest's offset in region 3, `map_info` the CPU
/// caching RM gave the memory. The guest writes through it into the very
/// object, an unmap takes the view away, and the resource (with the object)
/// lives on until UNREF.
#[test]
fn a_mappable_sysmem_rm_blob_maps_into_region_3() {
    let mut t = Rig::new();
    let object = 1u64 << 20;
    t.rm.objects.insert((7, 77), (object, Some(0)));
    t.rm.placements.insert((7, 77), SYS_CACHED);
    t.rm.objects.insert((7, 78), (object, Some(0)));
    t.rm.placements.insert((7, 78), SYS_WC);
    t.rm.objects.insert((7, 79), (object, Some(0)));
    t.rm.placements.insert((7, 79), VIDMEM);
    t.ctx(1);
    let mappable = |id, gem, size| ResourceCreateBlob {
        blob_flags: BLOB_FLAG_USE_MAPPABLE | BLOB_FLAG_USE_SHAREABLE,
        ..rm_blob_cmd(1, id, 7, gem, size)
    };
    // Video memory is behind BAR1: not mappable this way.
    assert_eq!(
        refusal(&mut t, &mappable(52, 79, object)),
        (RESP_ERR_INVALID_PARAMETER, libc::EOPNOTSUPP)
    );
    // Any size up to the object's; the mapping covers whole pages.
    assert_eq!(
        t.ty(&mappable(50, 77, object - 100).to_bytes()),
        RESP_OK_NODATA
    );
    assert_eq!(t.ty(&mappable(51, 78, 8192).to_bytes()), RESP_OK_NODATA);

    // MAP_BLOB: the offset's checks are region 3's; map_info is the caching.
    assert_eq!(t.map(50, 100), RESP_ERR_INVALID_PARAMETER);
    let (h, body) = t.send(&map_cmd(50, 1 << 20));
    assert_eq!(h.ty, RESP_OK_MAP_INFO);
    assert_eq!(
        u32::from_le_bytes(body[0..4].try_into().unwrap()) & MAP_CACHE_MASK,
        MAP_CACHE_CACHED
    );
    let (h, body) = t.send(&map_cmd(51, 4 << 20));
    assert_eq!(h.ty, RESP_OK_MAP_INFO);
    assert_eq!(
        u32::from_le_bytes(body[0..4].try_into().unwrap()) & MAP_CACHE_MASK,
        MAP_CACHE_WC
    );
    assert_eq!(
        t.region.calls(),
        vec![("place", 1 << 20, object), ("place", 4 << 20, 8192)]
    );

    // Writes through region 3 land in the object (the backend's own fd).
    let at = t.region.base + (1 << 20);
    // SAFETY: inside the placed mapping.
    unsafe { std::ptr::write_volatile((at + 4096) as *mut u32, 0x00c0_ffee) };
    let fd = t.venus.resources[&50].fd.as_raw_fd();
    let mut word = [0u8; 4];
    // SAFETY: a 4-byte read into a local from a descriptor we hold.
    assert_eq!(
        unsafe { libc::pread(fd, word.as_mut_ptr().cast(), 4, 4096) },
        4
    );
    assert_eq!(u32::from_le_bytes(word), 0x00c0_ffee);

    // UNMAP takes the view away; the resource and its object stay.
    assert_eq!(
        t.ty(&res_cmd(CMD_RESOURCE_UNMAP_BLOB, 0, 50)),
        RESP_OK_NODATA
    );
    assert_eq!(
        t.region.calls().last(),
        Some(&("withdraw", 1 << 20, object))
    );
    assert_eq!(t.venus.resources(), 2);
    assert_eq!(
        unsafe { libc::pread(fd, word.as_mut_ptr().cast(), 4, 4096) },
        4,
        "still the backend's"
    );
    // Mapped again (elsewhere), then UNREF while mapped: unmapped first.
    assert_eq!(t.map(50, 8 << 20), RESP_OK_MAP_INFO);
    assert_eq!(t.ty(&res_cmd(CMD_RESOURCE_UNREF, 0, 50)), RESP_OK_NODATA);
    assert_eq!(
        t.region.calls().last(),
        Some(&("withdraw", 8 << 20, object))
    );
    assert_eq!(t.venus.mappings(), 1);

    // Without MAPPABLE an RM blob still cannot be mapped.
    assert_eq!(
        t.ty(&rm_blob_cmd(1, 53, 7, 77, 4096).to_bytes()),
        RESP_OK_NODATA
    );
    assert_eq!(t.map(53, 16 << 20), RESP_ERR_INVALID_PARAMETER);
}
