// SPDX-License-Identifier: Apache-2.0
//
// The boot console against an in-process fake VNC server and a fake viewer
// (the broker end of a socketpair adopted by the display link).

use super::*;
use crate::display::{CONSOLE_GRACE, wire};
use protocol::messages::ScanoutFlip;
use std::os::unix::net::UnixListener;

// -- input ------------------------------------------------------------------

fn ev(t: u16, c: u16, v: i32) -> InputEventEntry {
    InputEventEntry::new(t, c, v)
}

fn syn() -> InputEventEntry {
    ev(input::EV_SYN, input::SYN_REPORT, 0)
}

const VGA: Sizes = Sizes::console((640, 480));

#[test]
fn keys_become_qemu_extended_key_events_with_qnums() {
    // evdev code, qnum, keysym
    let cases: [(u16, u32, u32); 6] = [
        (30, 0x1e, 0x61),    // KEY_A, keysym 'a' (never 'A': QEMU reads case as caps lock)
        (28, 0x1c, 0xff0d),  // KEY_ENTER
        (42, 0x2a, 0xffe1),  // KEY_LEFTSHIFT
        (60, 0x3c, 0xffbf),  // KEY_F2
        (111, 0xd3, 0xffff), // KEY_DELETE (an 0xe0 code: 0x80 | 0x53)
        (103, 0xc8, 0xff52), // KEY_UP (0x80 | 0x48)
    ];
    for (code, qnum, sym) in cases {
        let mut e = InputEncoder::default();
        let mut out = Vec::new();
        e.event(&ev(input::EV_KEY, code, 1), VGA, true, &mut out);
        e.event(&syn(), VGA, true, &mut out);
        assert_eq!(out, rfb::qemu_key_event(true, sym, qnum), "code {code}");
        assert_eq!(keymap::qnum(code), Some(qnum as u8));
        out.clear();
        e.event(&ev(input::EV_KEY, code, 0), VGA, true, &mut out);
        assert_eq!(out, rfb::qemu_key_event(false, sym, qnum), "code {code}");
    }
}

#[test]
fn held_keys_are_released_and_strays_dropped() {
    let mut e = InputEncoder::default();
    let mut out = Vec::new();
    // A release for a key pressed while the guest had the input: dropped.
    e.event(&ev(input::EV_KEY, 30, 0), VGA, true, &mut out);
    assert!(out.is_empty());
    e.event(&ev(input::EV_KEY, 42, 1), VGA, true, &mut out);
    e.event(&ev(input::EV_KEY, input::KEY_MAX, 1), VGA, true, &mut out);
    e.event(&ev(input::EV_KEY, BTN_LEFT, 1), VGA, true, &mut out);
    assert_eq!(e.held(), 2);
    out.clear();
    e.release_all(true, &mut out);
    // Shift up (KEY_MAX has no qnum: nothing to send), then the button.
    let mut want = rfb::qemu_key_event(false, 0xffe1, 0x2a).to_vec();
    want.extend_from_slice(&rfb::pointer_event(0, 0, 0));
    assert_eq!(out, want);
    assert_eq!(e.held(), 0);
    // Without the extension, keysyms.
    out.clear();
    e.event(&ev(input::EV_KEY, 28, 1), VGA, false, &mut out);
    assert_eq!(out, rfb::key_event(true, 0xff0d));
}

#[test]
fn the_pointer_scales_to_the_framebuffer_with_a_button_mask() {
    assert_eq!(InputEncoder::scale(0, 640), 0);
    assert_eq!(InputEncoder::scale(INPUT_ABS_MAX, 640), 639);
    assert_eq!(InputEncoder::scale(INPUT_ABS_MAX / 2, 641), 320);
    assert_eq!(InputEncoder::scale(-5, 640), 0);
    assert_eq!(InputEncoder::scale(INPUT_ABS_MAX + 9, 480), 479);

    let size = Sizes::console((800, 600));
    let mut e = InputEncoder::default();
    let mut out = Vec::new();
    e.event(
        &ev(input::EV_ABS, input::ABS_X, INPUT_ABS_MAX),
        size,
        true,
        &mut out,
    );
    e.event(
        &ev(input::EV_ABS, input::ABS_Y, INPUT_ABS_MAX / 2),
        size,
        true,
        &mut out,
    );
    assert!(out.is_empty(), "motion goes at the SYN");
    e.event(&syn(), size, true, &mut out);
    assert_eq!(out, rfb::pointer_event(0, 799, 299));

    out.clear();
    e.event(&ev(input::EV_KEY, BTN_LEFT, 1), size, true, &mut out);
    e.event(&ev(input::EV_KEY, BTN_RIGHT, 1), size, true, &mut out);
    e.event(&ev(input::EV_KEY, BTN_MIDDLE, 1), size, true, &mut out);
    e.event(&ev(input::EV_KEY, BTN_RIGHT, 0), size, true, &mut out);
    let masks: Vec<u8> = out.chunks(6).map(|m| m[1]).collect();
    assert_eq!(
        masks,
        [
            MASK_LEFT,
            MASK_LEFT | MASK_RIGHT,
            MASK_LEFT | MASK_RIGHT | MASK_MIDDLE,
            MASK_LEFT | MASK_MIDDLE
        ]
    );

    // A wheel detent: press and release of button 4 (up) or 5 (down), with
    // the held buttons kept; the hi-res twin is ignored.
    out.clear();
    e.event(
        &ev(input::EV_REL, input::REL_WHEEL, 1),
        size,
        true,
        &mut out,
    );
    e.event(
        &ev(input::EV_REL, input::REL_WHEEL_HI_RES, 120),
        size,
        true,
        &mut out,
    );
    e.event(
        &ev(input::EV_REL, input::REL_WHEEL, -1),
        size,
        true,
        &mut out,
    );
    e.event(&syn(), size, true, &mut out);
    let held = MASK_LEFT | MASK_MIDDLE;
    let masks: Vec<u8> = out.chunks(6).map(|m| m[1]).collect();
    assert_eq!(
        masks,
        [held | MASK_WHEEL_UP, held, held | MASK_WHEEL_DOWN, held]
    );

    // Relative motion (a grabbed pointer) moves within the framebuffer.
    out.clear();
    e.event(&ev(input::EV_REL, input::REL_X, 10), size, true, &mut out);
    e.event(&ev(input::EV_REL, input::REL_Y, -400), size, true, &mut out);
    e.event(&syn(), size, true, &mut out);
    assert_eq!(out, rfb::pointer_event(held, 799, 0));
}

// -- a fake VNC server ------------------------------------------------------

struct Server {
    dir: PathBuf,
    path: PathBuf,
    l: UnixListener,
}

impl Server {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("nvgpu-console-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("vnc.sock");
        let _ = std::fs::remove_file(&path);
        let l = UnixListener::bind(&path).unwrap();
        Self { dir, path, l }
    }

    /// Accept the console and do the server's half of the handshake.
    fn accept(&self, w: u16, h: u16) -> Conn {
        let (mut s, _) = self.l.accept().unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        s.write_all(rfb::VERSION).unwrap();
        let mut v = [0u8; 12];
        s.read_exact(&mut v).unwrap();
        assert_eq!(&v, rfb::VERSION);
        s.write_all(&[1, rfb::SEC_NONE]).unwrap();
        let mut b = [0u8; 1];
        s.read_exact(&mut b).unwrap();
        assert_eq!(b[0], rfb::SEC_NONE);
        s.write_all(&0u32.to_be_bytes()).unwrap();
        s.read_exact(&mut b).unwrap(); // ClientInit
        let mut init = Vec::new();
        init.extend_from_slice(&w.to_be_bytes());
        init.extend_from_slice(&h.to_be_bytes());
        init.extend_from_slice(&[0u8; 16]); // the server's own pixel format
        init.extend_from_slice(&4u32.to_be_bytes());
        init.extend_from_slice(b"QEMU");
        s.write_all(&init).unwrap();
        let mut c = Conn { s };
        let pf = c.msg();
        assert_eq!(pf, Msg::PixelFormat(rfb::set_pixel_format().to_vec()));
        assert_eq!(c.msg(), Msg::Encodings(rfb::ENCODINGS.to_vec()));
        c
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Msg {
    PixelFormat(Vec<u8>),
    Encodings(Vec<i32>),
    Request { incremental: bool, w: u16, h: u16 },
    Other(Vec<u8>),
}

/// x, y, w, h, encoding, data.
type RectSpec = (u16, u16, u16, u16, i32, Vec<u8>);

struct Conn {
    s: UnixStream,
}

impl Conn {
    fn exact(&mut self, n: usize) -> Vec<u8> {
        let mut b = vec![0u8; n];
        self.s.read_exact(&mut b).unwrap();
        b
    }

    /// The next client message.
    fn msg(&mut self) -> Msg {
        let t = self.exact(1)[0];
        match t {
            0 => Msg::PixelFormat([vec![0], self.exact(19)].concat()),
            2 => {
                let h = self.exact(3);
                let n = u16::from_be_bytes([h[1], h[2]]) as usize;
                let e = self.exact(4 * n);
                Msg::Encodings(
                    e.chunks(4)
                        .map(|c| i32::from_be_bytes(c.try_into().unwrap()))
                        .collect(),
                )
            }
            3 => {
                let b = self.exact(9);
                Msg::Request {
                    incremental: b[0] != 0,
                    w: u16::from_be_bytes([b[5], b[6]]),
                    h: u16::from_be_bytes([b[7], b[8]]),
                }
            }
            4 => Msg::Other([vec![4], self.exact(7)].concat()),
            5 => Msg::Other([vec![5], self.exact(5)].concat()),
            255 => Msg::Other([vec![255], self.exact(11)].concat()),
            t => panic!("client message {t}"),
        }
    }

    /// The next update request, skipping input.
    fn request(&mut self) -> Msg {
        loop {
            let m = self.msg();
            if matches!(m, Msg::Request { .. }) {
                return m;
            }
        }
    }

    fn update(&mut self, rects: &[RectSpec]) {
        let mut m = vec![0u8, 0];
        m.extend_from_slice(&(rects.len() as u16).to_be_bytes());
        for (x, y, w, h, enc, data) in rects {
            for f in [x, y, w, h] {
                m.extend_from_slice(&f.to_be_bytes());
            }
            m.extend_from_slice(&enc.to_be_bytes());
            m.extend_from_slice(data);
        }
        self.s.write_all(&m).unwrap();
    }

    /// No message within `d`.
    fn quiet_for(&mut self, d: Duration) -> bool {
        self.s.set_read_timeout(Some(d)).unwrap();
        let mut b = [0u8; 1];
        let r = self.s.read(&mut b);
        self.s
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        match r {
            Err(e) => matches!(
                e.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ),
            Ok(_) => false,
        }
    }
}

fn pixels(w: u16, h: u16, seed: u8) -> Vec<u8> {
    (0..w as usize * h as usize * 4)
        .map(|i| (i as u8).wrapping_mul(7).wrapping_add(seed))
        .collect()
}

// -- a fake viewer -----------------------------------------------------------

fn socketpair() -> (OwnedFd, OwnedFd) {
    let mut sv = [0i32; 2];
    // SAFETY: plain socketpair.
    let r = unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
            0,
            sv.as_mut_ptr(),
        )
    };
    assert_eq!(r, 0);
    // SAFETY: fresh descriptors.
    unsafe { (OwnedFd::from_raw_fd(sv[0]), OwnedFd::from_raw_fd(sv[1])) }
}

/// One 40-byte record from the link, with any fd, waiting up to 5 s.
fn viewer_recv(fd: RawFd) -> (wire::Cmd, Option<OwnedFd>) {
    let mut p = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one live pollfd.
    assert_eq!(
        unsafe { libc::poll(&mut p, 1, 5000) },
        1,
        "no record from the link"
    );
    let mut buf = [0u8; wire::CMD_SIZE];
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    let mut cbuf = [0u64; 8];
    // SAFETY: a zeroed msghdr, then pointed at live buffers.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.as_mut_ptr().cast();
    msg.msg_controllen = std::mem::size_of_val(&cbuf);
    // SAFETY: as above.
    let n = unsafe { libc::recvmsg(fd, &mut msg, libc::MSG_CMSG_CLOEXEC) };
    assert_eq!(n, wire::CMD_SIZE as isize);
    let mut got = None;
    // SAFETY: walking the control buffer recvmsg filled.
    unsafe {
        let c = libc::CMSG_FIRSTHDR(&msg);
        if !c.is_null() && (*c).cmsg_type == libc::SCM_RIGHTS {
            let f = std::ptr::read_unaligned(libc::CMSG_DATA(c) as *const RawFd);
            got = Some(OwnedFd::from_raw_fd(f));
        }
    }
    (wire::Cmd::decode(&buf), got)
}

fn viewer_quiet(fd: RawFd, ms: i32) -> bool {
    let mut p = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one live pollfd.
    unsafe { libc::poll(&mut p, 1, ms) == 0 }
}

/// The next frame the viewer is sent: the ATTACH (with its fd), after any
/// WINDOW, and the COMMIT. Returns the ATTACH, its fd and the WINDOW size.
fn viewer_frame(fd: RawFd) -> (wire::Cmd, OwnedFd, Option<(u32, u32)>) {
    let mut window = None;
    loop {
        let (c, f) = viewer_recv(fd);
        match c.ty {
            wire::CMD_WINDOW => window = Some((c.width, c.height)),
            wire::CMD_ATTACH => {
                assert_eq!(viewer_recv(fd).0.ty, wire::CMD_COMMIT);
                return (c, f.expect("ATTACH carries the buffer"), window);
            }
            wire::CMD_CAPS | wire::CMD_CURSOR | wire::CMD_QUERY_FORMAT => {}
            t => panic!("unexpected record {t}"),
        }
    }
}

/// Check a console frame: shared memory, XRGB8888 linear, tight stride, a
/// memfd sealed against shrinking, holding `want`.
fn check_shm(c: &wire::Cmd, f: &OwnedFd, w: u32, h: u32, want: &[u8]) {
    assert_eq!(c.flags, wire::CMD_F_SHM);
    assert_eq!((c.width, c.height, c.stride, c.offset), (w, h, w * 4, 0));
    assert_eq!((c.fourcc, c.modifier), (XRGB8888, 0));
    // SAFETY: fcntl and pread on a descriptor we own, into a live buffer.
    unsafe {
        let seals = libc::fcntl(f.as_raw_fd(), libc::F_GET_SEALS);
        assert!(
            seals >= 0 && seals & libc::F_SEAL_SHRINK != 0,
            "seals {seals:#x}"
        );
        assert_eq!(
            libc::lseek(f.as_raw_fd(), 0, libc::SEEK_END),
            (w * h * 4) as i64
        );
        let mut got = vec![0u8; want.len()];
        let n = libc::pread(f.as_raw_fd(), got.as_mut_ptr().cast(), got.len(), 0);
        assert_eq!(n, want.len() as isize);
        assert_eq!(got, want);
    }
}

fn guest_flip(link: &DisplayLink, buf: &OwnedFd) -> FlipOutcome {
    let f = ScanoutFlip {
        width: 64,
        height: 32,
        stride: 256,
        fourcc: XRGB8888,
        ..Default::default()
    };
    link.flip(buf.as_raw_fd(), &f)
}

fn memfd() -> OwnedFd {
    // SAFETY: plain memfd_create.
    let fd = unsafe { libc::memfd_create(c"guest".as_ptr(), libc::MFD_CLOEXEC) };
    assert!(fd >= 0);
    // SAFETY: a fresh descriptor.
    unsafe { OwnedFd::from_raw_fd(fd) }
}

fn wait_for(mut f: impl FnMut() -> bool) -> bool {
    let until = Instant::now() + Duration::from_secs(5);
    while Instant::now() < until {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    false
}

/// A link with one viewer that has said HELLO, and a console on `server`.
fn setup(server: &Server) -> (Arc<DisplayLink>, OwnedFd, Console) {
    let link = DisplayLink::new(None);
    let (ours, viewer) = socketpair();
    link.adopt(ours);
    link.hello_for_test(0);
    let console = Console::start(link.clone(), server.path.clone(), None).unwrap();
    (link, viewer, console)
}

#[test]
fn frames_reach_the_viewer_as_sealed_shared_memory_and_follow_resizes() {
    let server = Server::new("frames");
    let (link, viewer, console) = setup(&server);
    assert!(link.console_shown(), "shown from the start");
    let mut c = server.accept(4, 3);
    assert_eq!(
        c.request(),
        Msg::Request {
            incremental: false,
            w: 4,
            h: 3
        }
    );
    let px = pixels(4, 3, 1);
    c.update(&[
        (0, 0, 0, 0, rfb::ENC_QEMU_EXT_KEY, vec![]),
        (0, 0, 4, 3, rfb::ENC_RAW, px.clone()),
    ]);
    let (a, f, win) = viewer_frame(viewer.as_raw_fd());
    assert_eq!(win, Some((4, 3)));
    check_shm(&a, &f, 4, 3, &px);

    // Paced incremental requests follow; a partial update lands in the
    // other buffer, with everything else kept.
    assert_eq!(
        c.request(),
        Msg::Request {
            incremental: true,
            w: 4,
            h: 3
        }
    );
    c.update(&[(1, 1, 2, 1, rfb::ENC_RAW, vec![0xaa; 8])]);
    let (a2, f2, win) = viewer_frame(viewer.as_raw_fd());
    assert_eq!(win, None, "same size, no WINDOW");
    let mut want = px.clone();
    want[(4 + 1) * 4..(4 + 3) * 4].fill(0xaa);
    check_shm(&a2, &f2, 4, 3, &want);
    // SAFETY: fstat on descriptors we own.
    let ino = |fd: &OwnedFd| unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        libc::fstat(fd.as_raw_fd(), &mut st);
        st.st_ino
    };
    assert_ne!(ino(&f), ino(&f2), "double-buffered");

    // The VM's screen resizes: a full request at the new size, and a
    // WINDOW before the new frame.
    assert!(matches!(
        c.request(),
        Msg::Request {
            incremental: true,
            ..
        }
    ));
    c.update(&[(0, 0, 6, 2, rfb::ENC_DESKTOP_SIZE, vec![])]);
    assert_eq!(
        c.request(),
        Msg::Request {
            incremental: false,
            w: 6,
            h: 2
        }
    );
    let px = pixels(6, 2, 9);
    c.update(&[(0, 0, 6, 2, rfb::ENC_RAW, px.clone())]);
    let (a4, f4, win) = viewer_frame(viewer.as_raw_fd());
    assert_eq!(win, Some((6, 2)));
    check_shm(&a4, &f4, 6, 2, &px);
    console.stop();
}

#[test]
fn the_guest_takes_over_on_its_flip_and_gives_back_on_disable_and_reset() {
    let server = Server::new("switch");
    let (link, viewer, console) = setup(&server);
    let mut c = server.accept(4, 3);
    assert!(matches!(
        c.request(),
        Msg::Request {
            incremental: false,
            ..
        }
    ));
    c.update(&[(0, 0, 4, 3, rfb::ENC_RAW, pixels(4, 3, 0))]);
    let (a, _, _) = viewer_frame(viewer.as_raw_fd());
    assert_eq!(a.flags, wire::CMD_F_SHM);
    assert!(matches!(
        c.request(),
        Msg::Request {
            incremental: true,
            ..
        }
    ));

    // The guest's first flip: its dma-buf replaces the console at once.
    let buf = memfd();
    assert_eq!(guest_flip(&link, &buf), FlipOutcome::Sent);
    assert_eq!(link.console_mode(), Some(ConsoleMode::Guest));
    assert!(!link.console_shown());
    let (a, _, win) = viewer_frame(viewer.as_raw_fd());
    assert_eq!((a.flags, a.width, win), (0, 64, Some((64, 32))));
    // The request that was out is answered: nothing reaches the viewer, and
    // no more requests are made.
    c.update(&[(0, 0, 4, 3, rfb::ENC_RAW, pixels(4, 3, 5))]);
    assert!(
        c.quiet_for(Duration::from_millis(150)),
        "no requests while hidden"
    );
    assert!(
        viewer_quiet(viewer.as_raw_fd(), 50),
        "nothing published while hidden"
    );
    // A console frame offered now is refused.
    let f = ShmFrame::new(4, 3).unwrap();
    assert_eq!(
        link.flip_console(f.fd(), &f.geometry()),
        FlipOutcome::NoBroker
    );

    // The guest turns its scanout off: after the grace period the console
    // is back, asks for a whole frame, and shows it.
    let t0 = Instant::now();
    link.disable();
    assert!(matches!(link.console_mode(), Some(ConsoleMode::Pending(_))));
    assert_eq!(
        c.request(),
        Msg::Request {
            incremental: false,
            w: 4,
            h: 3
        }
    );
    assert!(t0.elapsed() >= CONSOLE_GRACE);
    assert!(link.console_shown());
    let px = pixels(4, 3, 7);
    c.update(&[(0, 0, 4, 3, rfb::ENC_RAW, px.clone())]);
    let (a, f, win) = viewer_frame(viewer.as_raw_fd());
    assert_eq!(win, Some((4, 3)), "the size is asked again after a disable");
    check_shm(&a, &f, 4, 3, &px);

    // A guest flip inside the grace period keeps the guest.
    assert!(matches!(
        c.request(),
        Msg::Request {
            incremental: true,
            ..
        }
    ));
    assert_eq!(guest_flip(&link, &buf), FlipOutcome::Sent);
    viewer_frame(viewer.as_raw_fd());
    link.disable();
    assert_eq!(guest_flip(&link, &buf), FlipOutcome::Sent);
    viewer_frame(viewer.as_raw_fd());
    std::thread::sleep(CONSOLE_GRACE + Duration::from_millis(50));
    assert_eq!(link.console_mode(), Some(ConsoleMode::Guest));

    // A device reset (guest reboot): the console at once.
    link.console_reset("test reset");
    assert!(link.console_shown());
    assert_eq!(
        c.request(),
        Msg::Request {
            incremental: false,
            w: 4,
            h: 3
        }
    );
    console.stop();
}

#[test]
fn the_console_reconnects_after_the_server_goes() {
    let server = Server::new("reconnect");
    let (_link, viewer, console) = setup(&server);
    let mut c = server.accept(4, 3);
    assert!(matches!(
        c.request(),
        Msg::Request {
            incremental: false,
            ..
        }
    ));
    // QEMU goes away (a VM restart) and comes back with another size.
    drop(c);
    let mut c = server.accept(8, 2);
    assert_eq!(
        c.request(),
        Msg::Request {
            incremental: false,
            w: 8,
            h: 2
        }
    );
    let px = pixels(8, 2, 3);
    c.update(&[(0, 0, 8, 2, rfb::ENC_RAW, px.clone())]);
    let (a, f, _) = viewer_frame(viewer.as_raw_fd());
    check_shm(&a, &f, 8, 2, &px);
    console.stop();
}

#[test]
fn the_console_waits_for_a_server_that_starts_later() {
    let dir = std::env::temp_dir().join(format!("nvgpu-console-late-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("vnc.sock");
    let _ = std::fs::remove_file(&path);
    let link = DisplayLink::new(None);
    let console = Console::start(link.clone(), path.clone(), None).unwrap();
    std::thread::sleep(Duration::from_millis(150));
    let server = Server {
        dir: dir.clone(),
        path: path.clone(),
        l: UnixListener::bind(&path).unwrap(),
    };
    let mut c = server.accept(4, 3);
    assert!(matches!(
        c.request(),
        Msg::Request {
            incremental: false,
            ..
        }
    ));
    console.stop();
}

#[test]
fn input_reaches_the_server_while_shown() {
    let server = Server::new("input");
    let (link, _viewer, console) = setup(&server);
    let mut c = server.accept(100, 50);
    assert!(matches!(c.request(), Msg::Request { .. }));
    // What the server announced decides the key encoding.
    c.update(&[
        (0, 0, 0, 0, rfb::ENC_QEMU_EXT_KEY, vec![]),
        (0, 0, 0, 0, rfb::ENC_LED_STATE, vec![0]),
    ]);
    std::thread::sleep(Duration::from_millis(30));
    let sink: Arc<dyn ConsoleSink> = console.shared.clone();
    sink.input(&[
        ev(input::EV_KEY, 60, 1),
        syn(),
        ev(input::EV_ABS, input::ABS_X, INPUT_ABS_MAX),
        ev(input::EV_ABS, input::ABS_Y, 0),
        syn(),
    ]);
    let mut got = Vec::new();
    while got.len() < 2 {
        if let Msg::Other(m) = c.msg() {
            got.push(m);
        }
    }
    assert_eq!(got[0], rfb::qemu_key_event(true, 0xffbf, 0x3c));
    assert_eq!(got[1], rfb::pointer_event(0, 99, 0));
    drop(link);
    console.stop();
}

/// The link thread routes input: to the console while it is shown, to the
/// guest otherwise, and what one side holds at the switch is released there.
#[test]
fn the_link_routes_input_by_who_owns_the_picture() {
    use crate::display::InputSink;
    struct Rec(Arc<Mutex<Vec<InputEventEntry>>>);
    impl ConsoleSink for Rec {
        fn input(&self, events: &[InputEventEntry]) {
            self.0.lock().unwrap().extend_from_slice(events);
        }
        fn wake(&self) {}
    }
    struct Guest(Arc<Mutex<Vec<InputEventEntry>>>);
    impl InputSink for Guest {
        fn push(&mut self, events: &[InputEventEntry]) -> usize {
            self.0.lock().unwrap().extend_from_slice(events);
            events.len()
        }
    }
    let dir = std::env::temp_dir().join(format!("nvgpu-console-route-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("broker.sock");
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).unwrap();
    let link = DisplayLink::new(Some(path.clone()));
    let to_console = Arc::new(Mutex::new(Vec::new()));
    let to_guest = Arc::new(Mutex::new(Vec::new()));
    link.attach_console(Arc::new(Rec(to_console.clone())));
    let stop = Arc::new(AtomicBool::new(false));
    let th = {
        let (link, g, stop) = (link.clone(), to_guest.clone(), stop.clone());
        std::thread::spawn(move || link.run(Box::new(Guest(g)), &stop))
    };
    let (mut viewer, _) = listener.accept().unwrap();
    let key = |code: i32, down: i32| wire::Pkt {
        ty: wire::EV_KEY,
        x: code,
        y: down,
        ..Default::default()
    };
    let hello = wire::Pkt {
        ty: wire::EV_HELLO,
        w0: wire::PROTO_VERSION,
        ..Default::default()
    };
    viewer.write_all(&hello.encode()).unwrap();
    viewer.write_all(&key(30, 1).encode()).unwrap();
    assert!(wait_for(|| to_console.lock().unwrap().len() == 2));
    assert!(to_guest.lock().unwrap().is_empty());

    // The guest flips with KEY_A held on the console: the console gets the
    // release, then the guest gets what follows.
    let buf = memfd();
    guest_flip(&link, &buf);
    viewer.write_all(&key(48, 1).encode()).unwrap();
    assert!(wait_for(|| to_guest.lock().unwrap().len() == 2));
    {
        let c = to_console.lock().unwrap();
        assert_eq!(c.len(), 4);
        assert_eq!(c[2], ev(input::EV_KEY, 30, 0));
    }
    assert_eq!(to_guest.lock().unwrap()[0], ev(input::EV_KEY, 48, 1));

    // A reset gives it back, KEY_B released to the guest.
    link.console_reset("test");
    viewer.write_all(&key(30, 1).encode()).unwrap();
    assert!(wait_for(|| to_console.lock().unwrap().len() == 6));
    assert_eq!(to_guest.lock().unwrap()[2], ev(input::EV_KEY, 48, 0));

    stop.store(true, Ordering::Relaxed);
    drop(viewer);
    th.join().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_guest_probe_that_fails_brings_the_console_back() {
    let link = DisplayLink::new(None);
    let alive = Arc::new(AtomicBool::new(true));
    let a = alive.clone();
    let path = std::env::temp_dir().join(format!("nvgpu-console-none-{}", std::process::id()));
    let console = Console::start(
        link.clone(),
        path,
        Some(Box::new(move || a.load(Ordering::Relaxed))),
    )
    .unwrap();
    let buf = memfd();
    guest_flip(&link, &buf);
    assert_eq!(link.console_mode(), Some(ConsoleMode::Guest));
    std::thread::sleep(PROBE_EVERY + Duration::from_millis(100));
    assert_eq!(link.console_mode(), Some(ConsoleMode::Guest));
    alive.store(false, Ordering::Relaxed);
    assert!(wait_for(|| link.console_shown()));
    console.stop();
}

#[test]
fn without_a_console_nothing_changes() {
    let link = DisplayLink::new(None);
    assert_eq!(link.console_mode(), None);
    assert!(!link.console_shown());
    link.disable();
    link.console_reset("no console");
    assert_eq!(link.console_mode(), None);
    assert_eq!(link.console_poll(), (false, None));
    let f = ShmFrame::new(2, 2).unwrap();
    assert_eq!(
        link.flip_console(f.fd(), &f.geometry()),
        FlipOutcome::NoBroker
    );
}

// -- input for a guest that takes no Conduit input (Windows) ----------------

#[test]
fn input_goes_to_the_console_by_whether_the_guest_takes_input() {
    // No console: always the guest, as before there was one.
    let link = DisplayLink::new(None);
    assert!(!link.input_to_console(true));
    assert!(!link.input_to_console(false));

    struct Nop;
    impl ConsoleSink for Nop {
        fn input(&self, _: &[InputEventEntry]) {}
        fn wake(&self) {}
    }
    link.attach_console(Arc::new(Nop));
    // The console shown: the console, whoever the guest is.
    assert!(link.input_to_console(true));
    assert!(link.input_to_console(false));
    // The guest's frames shown: the guest if it takes input, else the
    // console still.
    let buf = memfd();
    guest_flip(&link, &buf);
    assert!(!link.input_to_console(true));
    assert!(link.input_to_console(false));
    // A takeover pending: still the guest's frames.
    link.disable();
    assert!(!link.input_to_console(true));
    assert!(link.input_to_console(false));
    // Back to the console.
    link.console_reset("test");
    assert!(link.input_to_console(true));
    assert!(link.input_to_console(false));
}

#[test]
fn the_pointer_follows_the_guest_picture_over_a_smaller_framebuffer() {
    // QEMU's screen is 1280x800 behind a 2560x1440 guest picture.
    let sizes = Sizes {
        fb: (1280, 800),
        view: (2560, 1440),
    };
    let mut e = InputEncoder::default();
    let mut out = Vec::new();
    // Absolute positions are fractions of the picture: the same fraction of
    // the framebuffer, which QEMU scales onto the tablet's range.
    e.event(
        &ev(input::EV_ABS, input::ABS_X, INPUT_ABS_MAX),
        sizes,
        true,
        &mut out,
    );
    e.event(
        &ev(input::EV_ABS, input::ABS_Y, INPUT_ABS_MAX / 2),
        sizes,
        true,
        &mut out,
    );
    e.event(&syn(), sizes, true, &mut out);
    assert_eq!(out, rfb::pointer_event(0, 1279, 399));

    // Relative motion is in pixels of the picture: from the left edge,
    // half the guest's width is half the framebuffer's, and two guest
    // pixels are one framebuffer pixel.
    out.clear();
    e.event(&ev(input::EV_ABS, input::ABS_X, 0), sizes, true, &mut out);
    e.event(
        &ev(input::EV_REL, input::REL_X, 1280),
        sizes,
        true,
        &mut out,
    );
    e.event(&syn(), sizes, true, &mut out);
    assert_eq!(e.position().0, 640);
    let before = e.position().0;
    e.event(&ev(input::EV_REL, input::REL_X, 2), sizes, true, &mut out);
    assert_eq!(e.position().0, before + 1);
    // Small steps add up instead of rounding away.
    for _ in 0..10 {
        e.event(&ev(input::EV_REL, input::REL_X, 1), sizes, true, &mut out);
    }
    assert_eq!(e.position().0, before + 6);
    // And stay inside it.
    e.event(
        &ev(input::EV_REL, input::REL_Y, -5000),
        sizes,
        true,
        &mut out,
    );
    e.event(
        &ev(input::EV_REL, input::REL_X, 9000),
        sizes,
        true,
        &mut out,
    );
    assert_eq!(e.position(), (1279, 0));

    // The framebuffer resizes under a held position: the same fraction.
    let sizes = Sizes {
        fb: (640, 480),
        view: (2560, 1440),
    };
    out.clear();
    e.event(&syn(), sizes, true, &mut out);
    e.event(&ev(input::EV_KEY, BTN_LEFT, 1), sizes, true, &mut out);
    let mut want = rfb::pointer_event(0, 639, 0).to_vec();
    want.extend_from_slice(&rfb::pointer_event(MASK_LEFT, 639, 0));
    assert_eq!(out, want);
}

/// A console sink that records, and a guest sink whose `takes_input` the
/// test sets.
struct Rec(Arc<Mutex<Vec<InputEventEntry>>>);
impl ConsoleSink for Rec {
    fn input(&self, events: &[InputEventEntry]) {
        self.0.lock().unwrap().extend_from_slice(events);
    }
    fn wake(&self) {}
}
struct Guest {
    got: Arc<Mutex<Vec<InputEventEntry>>>,
    takes: Arc<AtomicBool>,
}
impl crate::display::InputSink for Guest {
    fn push(&mut self, events: &[InputEventEntry]) -> usize {
        self.got.lock().unwrap().extend_from_slice(events);
        events.len()
    }
    fn takes_input(&mut self) -> bool {
        self.takes.load(Ordering::Relaxed)
    }
}

/// The link thread routes a guest that takes no Conduit input to the console
/// before and after its frames take over; once it takes input, input follows
/// the picture, and on each switch what was held is released where it was.
#[test]
fn a_guest_without_conduit_input_keeps_the_console_input() {
    let dir = std::env::temp_dir().join(format!("nvgpu-console-noinput-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("broker.sock");
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).unwrap();
    let link = DisplayLink::new(Some(path.clone()));
    let to_console = Arc::new(Mutex::new(Vec::new()));
    let to_guest = Arc::new(Mutex::new(Vec::new()));
    let takes = Arc::new(AtomicBool::new(false));
    link.attach_console(Arc::new(Rec(to_console.clone())));
    let stop = Arc::new(AtomicBool::new(false));
    let th = {
        let sink = Guest {
            got: to_guest.clone(),
            takes: takes.clone(),
        };
        let (link, stop) = (link.clone(), stop.clone());
        std::thread::spawn(move || link.run(Box::new(sink), &stop))
    };
    let (mut viewer, _) = listener.accept().unwrap();
    let key = |code: i32, down: i32| wire::Pkt {
        ty: wire::EV_KEY,
        x: code,
        y: down,
        ..Default::default()
    };
    let pad = wire::Pkt {
        ty: wire::EV_PAD,
        x: 0x130, // BTN_SOUTH
        y: 1,
        w0: input::EV_KEY as u32,
        ..Default::default()
    };
    let hello = wire::Pkt {
        ty: wire::EV_HELLO,
        w0: wire::PROTO_VERSION,
        ..Default::default()
    };
    let console_len = || to_console.lock().unwrap().len();
    viewer.write_all(&hello.encode()).unwrap();

    // Before the guest's frames: the console.
    viewer.write_all(&key(30, 1).encode()).unwrap();
    assert!(wait_for(|| console_len() == 2));

    // The guest's frames take over with KEY_A held: nothing is released,
    // and what follows still goes to the console -- the release of KEY_A
    // too. A gamepad stays the guest's, which takes none: dropped.
    let buf = memfd();
    guest_flip(&link, &buf);
    assert!(!link.console_shown());
    viewer.write_all(&pad.encode()).unwrap();
    viewer.write_all(&key(30, 0).encode()).unwrap();
    viewer.write_all(&key(48, 1).encode()).unwrap();
    assert!(wait_for(|| console_len() == 6));
    {
        let c = to_console.lock().unwrap();
        assert_eq!(c[2], ev(input::EV_KEY, 30, 0));
        assert_eq!(c[4], ev(input::EV_KEY, 48, 1));
    }
    std::thread::sleep(Duration::from_millis(30));
    assert!(to_guest.lock().unwrap().is_empty(), "nothing for the guest");
    assert!(link.stats.input_dropped.load(Ordering::Relaxed) >= 1);

    // The guest starts taking input (a driver that posts event buffers)
    // while its frames are shown: KEY_B is released on the console, and the
    // guest gets what follows, gamepads included.
    takes.store(true, Ordering::Relaxed);
    viewer.write_all(&key(30, 1).encode()).unwrap();
    viewer.write_all(&pad.encode()).unwrap();
    assert!(wait_for(|| to_guest.lock().unwrap().len() == 3));
    assert_eq!(console_len(), 8);
    assert_eq!(to_console.lock().unwrap()[6], ev(input::EV_KEY, 48, 0));
    {
        let g = to_guest.lock().unwrap();
        assert_eq!(g[0], ev(input::EV_KEY, 30, 1));
        assert_eq!(g[2].ev_type, (1 << 8) | input::EV_KEY, "pad 0");
    }

    // It stops (the queue went with a reset): input is the console's again.
    // KEY_A's release is the guest's, which takes nothing now: dropped, not
    // kept for a later driver.
    let dropped = link.stats.input_dropped.load(Ordering::Relaxed);
    takes.store(false, Ordering::Relaxed);
    viewer.write_all(&key(31, 1).encode()).unwrap();
    assert!(wait_for(|| console_len() == 10));
    assert_eq!(to_console.lock().unwrap()[8], ev(input::EV_KEY, 31, 1));
    assert_eq!(to_guest.lock().unwrap().len(), 3);
    assert_eq!(
        link.stats.input_dropped.load(Ordering::Relaxed),
        dropped + 2
    );

    stop.store(true, Ordering::Relaxed);
    drop(viewer);
    th.join().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Hidden behind a guest that takes no Conduit input, the console still
/// sends input to the server, with relative motion measured against the
/// guest's picture rather than QEMU's screen.
#[test]
fn input_reaches_the_server_behind_the_guest_picture() {
    let server = Server::new("behind");
    let (link, _viewer, console) = setup(&server);
    let mut c = server.accept(100, 50);
    assert!(matches!(c.request(), Msg::Request { .. }));
    c.update(&[(0, 0, 0, 0, rfb::ENC_QEMU_EXT_KEY, vec![])]);
    // The guest's 64x32 frames take over.
    let buf = memfd();
    guest_flip(&link, &buf);
    assert!(!link.console_shown());
    assert_eq!(link.guest_picture_size(), Some((64, 32)));
    std::thread::sleep(Duration::from_millis(30));
    let sink: Arc<dyn ConsoleSink> = console.shared.clone();
    sink.input(&[
        ev(input::EV_KEY, 60, 1),
        syn(),
        ev(input::EV_ABS, input::ABS_X, 0),
        ev(input::EV_ABS, input::ABS_Y, 0),
        ev(input::EV_REL, input::REL_X, 32),
        ev(input::EV_REL, input::REL_Y, 31),
        syn(),
    ]);
    let mut got = Vec::new();
    while got.len() < 2 {
        if let Msg::Other(m) = c.msg() {
            got.push(m);
        }
    }
    assert_eq!(got[0], rfb::qemu_key_event(true, 0xffbf, 0x3c));
    // Half the guest's width is half QEMU's; its full height is QEMU's.
    assert_eq!(got[1], rfb::pointer_event(0, 50, 49));
    console.stop();
}
