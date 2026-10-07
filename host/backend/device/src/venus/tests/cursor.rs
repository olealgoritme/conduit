//! `CMD_SET_CURSOR_BLOB`: a Windows guest's hardware cursor
//! (docs/SCANOUT.md "Hardware cursor, Windows guests").

use super::*;

fn cursor_cmd(id: u32, w: u32, h: u32, offset: u32, visible: bool) -> [u8; SetCursorBlob::LEN] {
    SetCursorBlob {
        hdr: hdr(CMD_SET_CURSOR_BLOB, 0),
        scanout_id: 0,
        resource_id: id,
        width: w,
        height: h,
        format: format::B8G8R8A8_UNORM,
        stride: 1024,
        offset,
        hot_x: 1,
        hot_y: 2,
        x: 100,
        y: -3,
        flags: if visible { CURSOR_BLOB_F_VISIBLE } else { 0 },
    }
    .to_bytes()
}

const SLOT: u32 = 256 * 1024;

/// Not served until the backend says so (an older guest or a backend
/// without the cursor plane gets `RESP_ERR_UNSPEC` and draws its own).
#[test]
fn the_cursor_is_refused_until_enabled() {
    let mut t = Rig::new();
    t.ctx(1);
    assert_eq!(t.blob(1, 10, 2 * SLOT as u64), RESP_OK_NODATA);
    assert_eq!(t.ty(&cursor_cmd(10, 32, 32, 0, true)), RESP_ERR_UNSPEC);
    assert!(t.venus.enable_cursor());
    assert_eq!(t.ty(&cursor_cmd(10, 32, 32, 0, true)), RESP_OK_NODATA);
    // No display, no cursor.
    let mut v = Venus::new(Box::new(Shared::new()), HOSTMEM, None);
    assert!(!v.enable_cursor());
}

/// The blob is exported once; each update names its rectangle, and the
/// display link keeps it for whichever client takes cursors. Hidden is a
/// bare update; shown again names the same memory.
#[test]
fn a_cursor_blob_is_exported_once_and_reaches_the_display() {
    let mut t = Rig::new();
    let (link, _broker) = display();
    assert!(t.venus.enable_cursor());
    t.ctx(1);
    assert_eq!(t.blob(1, 10, 2 * SLOT as u64), RESP_OK_NODATA);

    // Slot 0, then slot 1 of the same blob (the KMD's ping-pong).
    assert_eq!(
        t.send_on(&cursor_cmd(10, 32, 32, 0, true), Some(&link))
            .0
            .ty,
        RESP_OK_NODATA
    );
    let (img, c) = link.kept_cursor_for_test().expect("kept");
    assert!(img && c.visible());
    assert_eq!(
        (c.width, c.height, c.stride, c.offset, c.hot_x, c.hot_y),
        (32, 32, 1024, 0, 1, 2)
    );
    assert_eq!((c.fourcc, c.modifier), (u32::from_le_bytes(*b"AR24"), 0));
    assert_eq!((c.host_handle, c.crtc_x, c.crtc_y), (10, 100, -3));
    assert_eq!(
        t.send_on(&cursor_cmd(10, 48, 40, SLOT, true), Some(&link))
            .0
            .ty,
        RESP_OK_NODATA
    );
    let (_, c2) = link.kept_cursor_for_test().unwrap();
    assert_eq!((c2.width, c2.height, c2.offset), (48, 40, SLOT));
    assert!(c2.seq != c.seq);
    assert_eq!(t.r.exports.lock().unwrap().len(), 1, "exported once");
    assert_eq!(t.venus.cursor_resource(), Some(10));

    // Hidden: no image, nothing exported.
    assert_eq!(
        t.send_on(&cursor_cmd(10, 48, 40, SLOT, false), Some(&link))
            .0
            .ty,
        RESP_OK_NODATA
    );
    let (img, c) = link.kept_cursor_for_test().unwrap();
    assert!(!img && !c.visible());
    assert_eq!(t.venus.cursor_resource(), None);
    // Resource 0 hides as well, whatever the flags say.
    assert_eq!(
        t.send_on(&cursor_cmd(0, 1, 1, 0, true), Some(&link)).0.ty,
        RESP_OK_NODATA
    );
    assert!(!link.kept_cursor_for_test().unwrap().0);

    // Shown again: the same export.
    assert_eq!(
        t.send_on(&cursor_cmd(10, 32, 32, 0, true), Some(&link))
            .0
            .ty,
        RESP_OK_NODATA
    );
    assert!(link.kept_cursor_for_test().unwrap().0);
    assert_eq!(t.r.exports.lock().unwrap().len(), 1);

    // The resource goes: the cursor is hidden with it.
    assert_eq!(
        t.send_on(&res_cmd(CMD_RESOURCE_UNREF, 0, 10), Some(&link))
            .0
            .ty,
        RESP_OK_NODATA
    );
    assert!(!link.kept_cursor_for_test().unwrap().0);
    assert_eq!(t.venus.cursor_resource(), None);
}

/// A device reset hides the cursor: its resource is gone, and the next
/// generation reuses the id.
#[test]
fn a_reset_hides_the_cursor() {
    let mut t = Rig::new();
    let (link, _broker) = display();
    assert!(t.venus.enable_cursor());
    t.ctx(1);
    assert_eq!(t.blob(1, 10, SLOT as u64), RESP_OK_NODATA);
    t.send_on(&cursor_cmd(10, 64, 64, 0, true), Some(&link));
    assert!(link.kept_cursor_for_test().unwrap().0);
    let _ = t.venus.reset(None, Some(&link));
    assert!(!link.kept_cursor_for_test().unwrap().0);
    assert_eq!(t.venus.cursor_resource(), None);
}

/// What the viewer cannot show is refused before anything is exported.
#[test]
fn bad_cursors_are_refused() {
    let mut t = Rig::new();
    let (link, _broker) = display();
    assert!(t.venus.enable_cursor());
    t.ctx(1);
    assert_eq!(t.blob(1, 10, SLOT as u64), RESP_OK_NODATA);
    let base = SetCursorBlob::from_bytes(&cursor_cmd(10, 32, 32, 0, true)).unwrap();
    let cases = [
        (
            SetCursorBlob {
                resource_id: 11,
                ..base
            },
            RESP_ERR_INVALID_RESOURCE_ID,
        ),
        (
            SetCursorBlob {
                scanout_id: 1,
                ..base
            },
            RESP_ERR_INVALID_SCANOUT_ID,
        ),
        (
            SetCursorBlob {
                format: format::B8G8R8X8_UNORM,
                ..base
            },
            RESP_ERR_INVALID_PARAMETER,
        ),
        (
            SetCursorBlob { width: 0, ..base },
            RESP_ERR_INVALID_PARAMETER,
        ),
        (
            SetCursorBlob {
                width: 257,
                stride: 2048,
                ..base
            },
            RESP_ERR_INVALID_PARAMETER,
        ),
        (
            SetCursorBlob {
                height: 257,
                ..base
            },
            RESP_ERR_INVALID_PARAMETER,
        ),
        (
            SetCursorBlob { hot_x: 32, ..base },
            RESP_ERR_INVALID_PARAMETER,
        ),
        (
            SetCursorBlob { hot_y: 32, ..base },
            RESP_ERR_INVALID_PARAMETER,
        ),
        (
            SetCursorBlob { stride: 64, ..base },
            RESP_ERR_INVALID_PARAMETER,
        ),
        // The last row runs past the blob.
        (
            SetCursorBlob {
                offset: SLOT - 1024 * 31,
                ..base
            },
            RESP_ERR_INVALID_PARAMETER,
        ),
    ];
    for (c, want) in cases {
        assert_eq!(t.send_on(&c.to_bytes(), Some(&link)).0.ty, want, "{c:?}");
    }
    assert!(t.r.exports.lock().unwrap().is_empty());
    assert!(link.kept_cursor_for_test().is_none());
    // The largest one that fits: 256 square, the last row ending at the end.
    let ok = SetCursorBlob {
        width: 256,
        height: 256,
        hot_x: 255,
        hot_y: 255,
        ..base
    };
    assert_eq!(t.send_on(&ok.to_bytes(), Some(&link)).0.ty, RESP_OK_NODATA);
    // A command of the wrong length is refused as every other.
    let mut long = ok.to_bytes().to_vec();
    long.push(0);
    assert_eq!(t.ty(&long), RESP_ERR_UNSPEC);
}
