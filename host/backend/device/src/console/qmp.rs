// SPDX-License-Identifier: Apache-2.0
//
// Relative pointer motion for a guest that takes its input from QEMU's
// emulated devices (Windows): QMP `input-send-event` on a monitor socket of
// its own, next to the VNC console socket.
//
// VNC cannot carry it. QEMU's VNC server sends absolute positions whenever
// an absolute pointer device exists (the USB tablet), and RFB has no
// relative mode of its own; the "pointer type change" pseudo-encoding only
// reports which kind QEMU is using. A first-person game reads raw relative
// motion, and the tablet has none: every report is a position.
//
// `input-send-event` without a `device` goes to QEMU's global input
// handlers, picked per event kind: `rel` reaches the PS/2 mouse (the only
// relative device, with no driver needed in the guest). `btn` reaches the
// first global pointer handler -- the tablet, unless the tablet is bound to
// the display (`display=video0`, which the VNC server then reaches first),
// in which case the PS/2 mouse gets the buttons too and a click never moves
// the pointer to the tablet's stale position. `conduit attach` binds it.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

/// The QMP socket next to the VNC console socket.
pub fn path_for(console: &Path) -> std::path::PathBuf {
    console.with_file_name("qmp.sock")
}

/// One `input-send-event` moving the relative pointer by (dx, dy).
pub fn rel_command(dx: i64, dy: i64) -> String {
    let mut ev = Vec::new();
    if dx != 0 {
        ev.push(format!(
            r#"{{"type":"rel","data":{{"axis":"x","value":{dx}}}}}"#
        ));
    }
    if dy != 0 {
        ev.push(format!(
            r#"{{"type":"rel","data":{{"axis":"y","value":{dy}}}}}"#
        ));
    }
    send_event(&ev)
}

/// One `input-send-event` pressing or releasing `buttons` in order (QMP
/// `InputButton` names: left, middle, right, wheel-up, wheel-down, side,
/// extra, wheel-left, wheel-right).
pub fn btn_command(buttons: &[(&str, bool)]) -> String {
    let ev: Vec<String> = buttons
        .iter()
        .map(|(b, down)| format!(r#"{{"type":"btn","data":{{"down":{down},"button":"{b}"}}}}"#))
        .collect();
    send_event(&ev)
}

fn send_event(ev: &[String]) -> String {
    format!(
        "{{\"execute\":\"input-send-event\",\"arguments\":{{\"events\":[{}]}}}}\n",
        ev.join(",")
    )
}

/// A QMP connection past its greeting and `qmp_capabilities`, nonblocking.
pub struct Qmp {
    pub sock: UnixStream,
    inbuf: Vec<u8>,
    /// Error replies seen (the first is logged).
    pub errors: u64,
}

impl Qmp {
    /// Connect and negotiate, within `timeout`.
    pub fn connect(path: &Path, timeout: Duration) -> io::Result<Self> {
        let sock = UnixStream::connect(path)?;
        sock.set_read_timeout(Some(timeout))?;
        sock.set_write_timeout(Some(timeout))?;
        let mut r = BufReader::new(sock.try_clone()?);
        let mut line = String::new();
        r.read_line(&mut line)?;
        if !line.contains("\"QMP\"") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("not a QMP greeting: {}", line.trim()),
            ));
        }
        (&sock).write_all(b"{\"execute\":\"qmp_capabilities\"}\n")?;
        loop {
            line.clear();
            if r.read_line(&mut line)? == 0 {
                return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
            }
            if line.contains("\"return\"") {
                break;
            }
            if line.contains("\"error\"") {
                return Err(io::Error::other(format!(
                    "qmp_capabilities: {}",
                    line.trim()
                )));
            }
            // An event before the reply: skipped.
        }
        if !r.buffer().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "QMP sent more than the capabilities reply",
            ));
        }
        sock.set_nonblocking(true)?;
        Ok(Self {
            sock,
            inbuf: Vec::new(),
            errors: 0,
        })
    }

    /// Read and drop replies and events; logs the first error reply.
    /// `Err` when the connection is gone.
    pub fn drain(&mut self) -> io::Result<()> {
        let mut chunk = [0u8; 4096];
        loop {
            match self.sock.read(&mut chunk) {
                Ok(0) => return Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
                Ok(n) => self.inbuf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        while let Some(i) = self.inbuf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.inbuf.drain(..=i).collect();
            if line.windows(7).any(|w| w == b"\"error\"") {
                if self.errors == 0 {
                    log::warn!(
                        "console: QMP refused relative input: {}",
                        String::from_utf8_lossy(&line).trim()
                    );
                }
                self.errors += 1;
            }
        }
        // A reply never runs to megabytes; a peer that sends one is not QMP.
        if self.inbuf.len() > 1 << 20 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "QMP reply too long",
            ));
        }
        Ok(())
    }
}
