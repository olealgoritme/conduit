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
mod sys;
#[cfg(windows)]
mod view;

#[cfg(windows)]
fn main() {
    app::run();
}

#[cfg(not(windows))]
fn main() {
    eprintln!("conduit-gpu-tray runs on Windows guests only");
}
