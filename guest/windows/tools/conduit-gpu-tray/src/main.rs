//! Conduit GPU: a tray app for Windows guests of a Conduit host. It reads the
//! host GPU's readings from the `org.conduit.stats.0` virtio-serial channel
//! and shows them as a live tray icon and a popup (click the icon).

#![cfg_attr(windows, windows_subsystem = "windows")]
#![allow(clippy::too_many_arguments, clippy::type_complexity)]

#[cfg(windows)]
mod app;
#[cfg(windows)]
mod apps;
#[cfg(windows)]
mod ctl;
#[cfg(windows)]
mod gfx;
#[cfg(windows)]
mod launch;
#[cfg(windows)]
mod sendto;
#[cfg(windows)]
mod shellmenu;
#[cfg(windows)]
mod sys;
#[cfg(windows)]
mod view;

#[cfg(windows)]
fn main() {
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    // Explorer's "Send to Conduit host" (conduit_shell_menu.dll) hands the
    // selection over in a list file.
    if args.len() == 2 && args[0] == "--send-list" {
        let code = sendto::send_list(std::path::Path::new(&args[1]));
        std::process::exit(code as i32);
    }
    app::run();
}

#[cfg(not(windows))]
fn main() {
    eprintln!("conduit-gpu-tray runs on Windows guests only");
}
