//! conduit-venus: the Venus renderer process (docs/VENUS.md).
//!
//! Listens on `--socket PATH`, accepts one backend, serves it until it hangs
//! up, then exits: one renderer process per VM, so a crashed or wedged GPU
//! context never outlives the VM that made it.

use conduit_venus::ipc::{IpcServer, listen_accept_one};
use conduit_venus::virgl::Virgl;
use std::path::PathBuf;
use std::process::ExitCode;

fn usage() -> ExitCode {
    eprintln!("usage: conduit-venus --socket PATH");
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let mut socket: Option<PathBuf> = None;
    let mut args = std::env::args_os().skip(1);
    while let Some(a) = args.next() {
        match a.to_str() {
            Some("--socket") => socket = args.next().map(PathBuf::from),
            Some("-h" | "--help") => {
                usage();
                return ExitCode::SUCCESS;
            }
            _ => return usage(),
        }
    }
    let Some(socket) = socket else { return usage() };

    // Vulkan comes up before listening: a host without a working driver
    // fails here, where the backend sees the process exit, rather than after
    // the guest has started talking Venus.
    let renderer = match Virgl::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("conduit-venus: {e}");
            return ExitCode::FAILURE;
        }
    };
    let sock = match listen_accept_one(&socket) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("conduit-venus: {}: {e}", socket.display());
            return ExitCode::FAILURE;
        }
    };
    match IpcServer::new(sock, Box::new(renderer)).serve() {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("conduit-venus: {e}");
            ExitCode::FAILURE
        }
    }
}
