//! conduit-venus: the Venus renderer process (docs/VENUS.md).
//!
//! Listens on `--socket PATH`, accepts one backend, serves it until it hangs
//! up, then exits: one renderer process per VM, so a crashed or wedged GPU
//! context never outlives the VM that made it.
//!
//! Sandboxed (src/sandbox.rs) before the first request is read; `--no-sandbox`
//! runs without Landlock and seccomp, for debugging only.

use conduit_venus::ipc::{IpcServer, listen_accept_one};
use conduit_venus::sandbox::{self, Sandbox};
use conduit_venus::virgl::Virgl;
use std::path::PathBuf;
use std::process::ExitCode;

fn usage() -> ExitCode {
    eprintln!("usage: conduit-venus --socket PATH [--vm NAME] [--no-sandbox]");
    eprintln!("       conduit-venus --sandbox-selftest [--vm NAME]");
    ExitCode::from(2)
}

fn fail(e: impl std::fmt::Display) -> ExitCode {
    eprintln!("conduit-venus: {e}");
    ExitCode::FAILURE
}

fn main() -> ExitCode {
    let mut socket: Option<PathBuf> = None;
    let mut vm: Option<String> = None;
    let mut no_sandbox = false;
    let mut selftest = false;
    let mut args = std::env::args_os().skip(1);
    while let Some(a) = args.next() {
        match a.to_str() {
            Some("--socket") => socket = args.next().map(PathBuf::from),
            Some("--vm") => vm = args.next().and_then(|v| v.into_string().ok()),
            Some("--no-sandbox") => no_sandbox = true,
            Some("--sandbox-selftest") => selftest = true,
            Some("-h" | "--help") => {
                usage();
                return ExitCode::SUCCESS;
            }
            _ => return usage(),
        }
    }
    if selftest && no_sandbox {
        return usage();
    }
    let socket = match socket {
        Some(s) => s,
        None if selftest => PathBuf::from("venus.sock"),
        None => return usage(),
    };

    let nofile = sandbox::raise_nofile();
    eprintln!("conduit-venus: open file limit {nofile}");

    // Root and CAP_SYS_ADMIN are refused with or without the sandbox; the
    // shader cache goes to this VM's own directory either way.
    let vm = sandbox::vm_name(vm.as_deref(), &socket);
    let cache = match sandbox::posture(&vm) {
        Ok(c) => c,
        Err(e) => return fail(e),
    };
    let sandbox = if no_sandbox {
        eprintln!("conduit-venus: ************************************************************");
        eprintln!("conduit-venus: WARNING: --no-sandbox: no Landlock, no seccomp. The guest's");
        eprintln!("conduit-venus: Vulkan reaches the host driver with every file and syscall");
        eprintln!("conduit-venus: this user has. For debugging only.");
        eprintln!("conduit-venus: ************************************************************");
        None
    } else {
        // Before virglrenderer starts a thread: environment for the driver,
        // and Landlock itself on a kernel without Landlock TSYNC.
        match Sandbox::prepare(&socket, cache) {
            Ok(s) => Some(s),
            Err(e) => return fail(e),
        }
    };

    // Vulkan comes up before listening: a host without a working driver
    // fails here, where the backend sees the process exit, rather than after
    // the guest has started talking Venus.
    let renderer = match Virgl::new() {
        Ok(r) => r,
        Err(e) => return fail(e),
    };

    if selftest {
        let Some(sb) = sandbox else { return usage() };
        let rules = sb.rules().clone();
        if let Err(e) = sb.enter() {
            return fail(e);
        }
        eprintln!("conduit-venus: sandbox self-test:");
        let bad = sandbox::selftest(&rules);
        drop(renderer);
        return if bad.is_empty() {
            eprintln!("conduit-venus: sandbox self-test PASS");
            ExitCode::SUCCESS
        } else {
            fail(format_args!("sandbox self-test FAIL: {}", bad.join("; ")))
        };
    }

    let sock = match listen_accept_one(&socket) {
        Ok(s) => s,
        Err(e) => return fail(format_args!("{}: {e}", socket.display())),
    };
    // The connection is accepted, no request read yet: confine every thread.
    if let Some(sb) = sandbox {
        match sb.enter() {
            Ok(r) => eprintln!(
                "conduit-venus: sandboxed: Landlock ABI {} ({}), seccomp denylist ({} insns), shader cache {}",
                r.landlock_abi,
                if r.landlock_all_threads { "all threads" } else { "applied before the first thread" },
                r.seccomp_insns,
                r.cache_dir.as_deref().map_or("off".into(), |d| d.display().to_string()),
            ),
            Err(e) => return fail(e),
        }
    }
    match IpcServer::new(sock, Box::new(renderer)).serve() {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => fail(e),
    }
}
