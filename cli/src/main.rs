//! conduit: share your NVIDIA GPU with a Linux VM and see its desktop in a window.

mod boot;
mod config;
mod create;
mod doctor;
mod guest;
mod host;
mod hypr;
mod libvirt;
mod lvrun;
mod mem;
mod mode;
mod net;
mod paths;
mod power;
mod qemu;
mod run;
mod scope;
mod stream;
mod sys;
mod trace;
mod ui;
mod units;
mod virt;
mod vm;

use anyhow::Result;
use clap::{Parser, Subcommand};
use mode::Mode;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "conduit",
    version,
    about = "Share your NVIDIA GPU with a Linux VM and see its desktop in a window",
    after_help = "Start here:\n  conduit doctor        check this computer\n  conduit create myvm   make a ready-to-use Ubuntu VM\n  conduit view myvm     open it in a window (closing the window shuts it down)"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
    /// `view`: clipboard sharing between your desktop and the VM's
    /// (text; needs conduit-clipboard-agent in the VM, docs/CLIPBOARD.md)
    #[arg(long, global = true, value_enum, default_value_t = run::Clipboard::Both)]
    clipboard: run::Clipboard,
    /// `up`/`view`: start even when the host looks short of free memory
    #[arg(long, global = true)]
    no_mem_check: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Make a new Ubuntu 24.04 VM with the Conduit guest driver installed
    Create {
        name: String,
        /// Maximum disk size (the file only grows as it fills)
        #[arg(long, default_value = "64G")]
        size: String,
        /// Desktop inside the VM
        #[arg(long, default_value = "gnome", value_parser = ["gnome", "xfce", "none"])]
        desktop: String,
        /// Memory for the VM
        #[arg(long, default_value = "4G")]
        ram: String,
        /// Number of CPU cores for the VM
        #[arg(long, default_value_t = 4)]
        cpus: u32,
        /// Login name inside the VM (default: your user name)
        #[arg(long)]
        user: Option<String>,
        /// Use an already downloaded noble-server-cloudimg-amd64-root.tar.xz (still verified)
        #[arg(long, value_name = "FILE")]
        tarball: Option<PathBuf>,
        /// Do not register the VM with libvirt (virt-manager / virsh)
        #[arg(long)]
        no_libvirt: bool,
    },
    /// Adopt an existing VM disk image (raw ext4), copying it (or --move)
    Import {
        path: PathBuf,
        name: String,
        /// Move the file instead of copying it
        #[arg(long = "move")]
        mv: bool,
        /// Memory for the VM
        #[arg(long, default_value = "4G")]
        ram: String,
        #[arg(long, default_value_t = 4)]
        cpus: u32,
        /// Login name inside the VM
        #[arg(long, default_value = "root")]
        user: String,
        /// Boot this kernel file (ELF vmlinux) instead of the one on the disk
        #[arg(long, value_name = "VMLINUX")]
        kernel: Option<PathBuf>,
        /// Share this NVIDIA user-space folder instead of staging the host's
        #[arg(long, value_name = "DIR")]
        share: Option<PathBuf>,
        /// VM network number N: the VM must use 172.30.N.2 (default: first free)
        #[arg(long, value_name = "N")]
        net: Option<u8>,
        /// Do not register the VM with libvirt (virt-manager / virsh)
        #[arg(long)]
        no_libvirt: bool,
    },
    /// Switch a VM to its distro's own kernel: installs linux-image-generic,
    /// headers, DKMS and the conduit-guest driver into its disk (VM stopped)
    #[command(name = "stock-kernel")]
    StockKernel { name: String },
    /// List your VMs
    List,
    /// Start a VM in the background (no window; `conduit view` opens one later)
    Up {
        name: String,
        /// Screen mode for the VM, e.g. 2560x1440@144 (default: your monitor's)
        #[arg(long, value_name = "WxH@HZ")]
        display: Option<String>,
        /// No screen at all (compute/ssh only)
        #[arg(long, conflicts_with = "display")]
        headless: bool,
        /// VM runner: qemu (default; has sound) or builtin (no sound)
        #[arg(long, value_enum)]
        vmm: Option<run::VmmKind>,
    },
    /// Open a VM in a window, starting it if needed. Closing the window shuts
    /// it down only if this command started it (see --keep-running)
    View {
        name: String,
        /// Screen mode, e.g. 1920x1080 or 2560x1440@240 (default: your monitor's)
        mode: Option<String>,
        /// Hyprland only: allow tearing + direct scanout while the viewer runs (restored after)
        #[arg(long)]
        tune_hyprland: bool,
        /// Start fullscreen (Ctrl+Alt+F toggles)
        #[arg(long)]
        fullscreen: bool,
        /// VM runner: qemu (default; has sound) or builtin (no sound)
        #[arg(long, value_enum)]
        vmm: Option<run::VmmKind>,
        /// Closing the window leaves the VM running even if this command started it
        #[arg(long)]
        keep_running: bool,
    },
    /// Shut a VM down cleanly (power button), forced off after the timeout,
    /// and stop everything that belongs to it
    Down {
        name: String,
        /// Seconds to wait for a clean shutdown before forcing it off
        #[arg(long, default_value_t = 30)]
        timeout: u64,
        /// Force it off now (like pulling the plug)
        #[arg(long)]
        force: bool,
    },
    /// Press the VM's power button and wait for it to turn off (never forced)
    Shutdown {
        name: String,
        /// Seconds to wait
        #[arg(long, default_value_t = 60)]
        timeout: u64,
    },
    /// Restart the VM's operating system cleanly
    Reboot { name: String },
    /// Hard reset (the reset button)
    Reset { name: String },
    /// Turn the VM off immediately (same as `down --force`)
    Poweroff { name: String },
    /// Pause the VM (its vCPUs stop; memory and GPU state stay)
    Pause { name: String },
    /// Continue a paused VM
    Resume { name: String },
    /// Show what is running
    Status { name: Option<String> },
    /// Show a VM's logs: backend, vm (console) or viewer
    Logs {
        name: String,
        #[arg(value_parser = ["backend", "vm", "viewer", "watcher", "share", "virtiofsd", "create"])]
        which: Option<String>,
        /// Keep printing new lines
        #[arg(short, long)]
        follow: bool,
        /// How many lines to show
        #[arg(short = 'n', long, default_value_t = 40)]
        lines: usize,
    },
    /// Open a terminal in a running VM (extra arguments are run as a command)
    Ssh {
        name: String,
        /// Log in as this user (default: the VM's user)
        #[arg(long, short)]
        user: Option<String>,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Give an existing libvirt / virt-manager VM Conduit's GPU (backs up its
    /// definition first; `conduit detach` restores it)
    Attach {
        name: String,
        /// Only print the changed VM definition
        #[arg(long)]
        dry_run: bool,
        /// libvirt connection, e.g. qemu:///system (default: whichever has the VM)
        #[arg(long, short = 'c')]
        connect: Option<String>,
        /// Do not install the guest driver now; print the command for later
        #[arg(long)]
        guest_later: bool,
    },
    /// Stream a VM to Moonlight and Conduit viewers over the network (NVENC).
    /// Also: `stream pair PIN`, `stream clients`, `stream unpair CLIENT`, `stream status`
    Stream {
        /// The VM, or one of: pair, clients, unpair, status, token
        target: String,
        /// The PIN (pair) or client name (unpair)
        arg: Option<String>,
        /// Defaults for what the client does not ask for: top (AV1 240 fps 200 Mbit/s), balanced, compat
        #[arg(long, default_value = "top")]
        preset: String,
        /// GameStream base port; for a second VM use another, e.g. 48089 (add the host in Moonlight as IP:48089)
        #[arg(long, default_value_t = 47989)]
        port: u16,
        /// Screen mode to start the VM with (the client's resolution takes over once connected)
        #[arg(long, value_name = "WxH@HZ")]
        display: Option<String>,
        /// Keep streaming as a systemd user service (restarts on failure, starts at login)
        #[arg(long)]
        service: bool,
        /// Stop the --service
        #[arg(long, conflicts_with = "service")]
        stop: bool,
        /// Also accept Conduit viewers over the network (`conduit remote`)
        #[arg(long)]
        link: bool,
        /// Offer video encryption to Moonlight clients that ask for it
        #[arg(long)]
        video_encryption: bool,
        /// Pace video bursts to this many Mbit/s (default 1000; 10000 on 10 GbE)
        #[arg(long)]
        link_mbps: Option<u32>,
        /// (internal) leave the VM running when streaming ends
        #[arg(long, hide = true)]
        keep_vm: bool,
    },
    /// Trace the GPU requests a running VM makes: record them, watch them
    /// live or summarise their latency (docs/TRACING.md).
    /// Also: `trace NAME status`, `trace NAME on|off` (the backend's own
    /// --trace file), `trace analyze FILE`
    Trace {
        /// The VM, or `analyze`
        target: String,
        /// For `analyze`: the trace file. For a VM: status, on or off
        arg: Option<String>,
        /// Print each request as it happens, coloured by latency
        #[arg(short, long)]
        follow: bool,
        /// Show only these: alloc, control, free, map, rm, uvm, nvkms, drm,
        /// open, close, mmap, munmap, event, errors, refused, slow:>1ms
        /// (comma-separated or repeated)
        #[arg(long, value_name = "WHAT")]
        filter: Vec<String>,
        /// When done, print counts, p50/p95/p99/max latency per call kind,
        /// the slowest RM controls and the errors
        #[arg(long)]
        summary: bool,
        /// Write the records to this file (default without --follow or
        /// --summary: conduit-trace-NAME-TIME.jsonl)
        #[arg(short, long, value_name = "FILE")]
        output: Option<PathBuf>,
        /// json or bin (default: from the file name; .bin is binary)
        #[arg(long, value_parser = ["json", "bin"])]
        format: Option<String>,
        /// Stop after this many seconds instead of at Ctrl-C
        #[arg(long, value_name = "SECS")]
        duration: Option<u64>,
    },
    /// Show a VM that another computer streams (`conduit stream NAME --link` there)
    Remote {
        /// The other computer: HOST or HOST:PORT (default port 48100)
        host: String,
        /// Its link token (`conduit stream token` there); remembered after the first connect
        #[arg(long)]
        token: Option<String>,
        /// Bit-exact pixels (HEVC 4:4:4 lossless) for 10 GbE links
        #[arg(long)]
        lossless: bool,
        /// h264, hevc or av1 (default: the host's preset)
        #[arg(long)]
        codec: Option<String>,
        /// Video bitrate, e.g. 300M (lossy only)
        #[arg(long)]
        bitrate: Option<String>,
        #[arg(long)]
        fps: Option<u32>,
        /// 4:4:4 chroma (lossy, sharper text)
        #[arg(long)]
        yuv444: bool,
        /// Start fullscreen (Ctrl+Alt+F toggles)
        #[arg(long)]
        fullscreen: bool,
    },
    /// Take Conduit's GPU off a libvirt VM: restore its original definition
    Detach { name: String },
    /// Make a Conduit VM a libvirt domain (virt-manager, virsh) or stop that
    Libvirt {
        #[arg(value_parser = ["enable", "disable"])]
        action: String,
        name: String,
    },
    /// Settings: `conduit config set view.close_stops_vm false`
    Config {
        #[arg(value_parser = ["get", "set", "unset"])]
        action: String,
        key: Option<String>,
        value: Option<String>,
    },
    /// Check this computer and explain how to fix problems (with NAME: also
    /// that VM's whole chain: libvirt domain, sockets, backend, guest driver)
    Doctor { name: Option<String> },
    /// (internal) viewer direct-mode hook for Hyprland
    #[command(hide = true)]
    HyprHook {
        action: String,
        #[arg(long)]
        state: PathBuf,
    },
    /// (internal) stop the VM when its viewer closes
    #[command(name = "_watch", hide = true)]
    Watch { name: String },
    /// (internal) conduit-backend@NAME.service: become the GPU backend
    #[command(name = "_backend", hide = true)]
    Backend { name: String },
    /// (internal) conduit-virtiofsd@NAME.service: become virtiofsd
    #[command(name = "_virtiofsd", hide = true)]
    Virtiofsd { name: String },
    /// (internal) after the backend stopped with its VM
    #[command(name = "_stopped", hide = true)]
    Stopped { name: String },
}

fn ram_mib(s: &str) -> Result<u64> {
    let b = ui::parse_size(s, 'G')?;
    if b < 512 << 20 {
        return Err(ui::oops(
            format!("{s} of memory is too little"),
            "Give the VM at least 512M; 4G is a good start",
        ));
    }
    Ok(b >> 20)
}

fn opt_mode(s: Option<&str>) -> Result<Option<Mode>> {
    s.map(str::parse).transpose()
}

fn list() -> Result<()> {
    let names = lvrun::all_names();
    if names.is_empty() {
        println!("No VMs yet. Create one with `conduit create myvm`.");
        return Ok(());
    }
    println!(
        "{:<20} {:<9} {:>9} {:>8} {:>5}  {:<8}  ADDRESS",
        "NAME", "STATE", "DISK", "RAM", "CPUS", "DESKTOP"
    );
    for n in names {
        if vm::VmConfig::load(&n).is_err() {
            if let Some(l) = virt::Link::load(&n) {
                println!(
                    "{:<20} {:<9} {:>9} {:>8} {:>5}  {:<8}  (libvirt VM {} on {})",
                    n,
                    lvrun::state_word(&n),
                    "-",
                    "-",
                    "-",
                    "attached",
                    l.domain,
                    l.uri
                );
                continue;
            }
        }
        match vm::VmConfig::load(&n) {
            Ok(c) => {
                let disk = std::fs::metadata(c.disk_path())
                    .map(|m| ui::human_bytes(m.blocks() * 512))
                    .unwrap_or_else(|_| "missing".into());
                let state = lvrun::state_word(&n);
                let state = if virt::Link::load(&n).is_some() {
                    format!("{state}*")
                } else {
                    state
                };
                println!(
                    "{:<20} {:<9} {:>9} {:>8} {:>5}  {:<8}  {}",
                    n,
                    state,
                    disk,
                    ui::human_bytes(c.ram_mib << 20),
                    c.cpus,
                    c.desktop,
                    c.net().guest_ip
                );
            }
            Err(e) => println!("{n:<20} (broken: {e})"),
        }
    }
    if names_have_libvirt() {
        println!("* also a libvirt VM: start/stop it in virt-manager (QEMU/KVM user session) or with virsh");
    }
    Ok(())
}

fn names_have_libvirt() -> bool {
    lvrun::all_names()
        .iter()
        .any(|n| virt::Link::load(n).is_some())
}

/// After `create`/`import`: register the VM with libvirt when it is there.
fn register(name: &str, no_libvirt: bool) -> anyhow::Result<()> {
    if no_libvirt {
        return Ok(());
    }
    if !virt::session_available() {
        ui::info("libvirt is not installed: the VM runs through `conduit up/view` only (install libvirt and run `conduit libvirt enable` to manage it from virt-manager)");
        return Ok(());
    }
    if let Err(e) = virt::enable(name) {
        ui::warn(format!("could not register {name} with libvirt: {e:#}"));
        ui::info(format!(
            "`conduit view {name}` works without it; retry with `conduit libvirt enable {name}`"
        ));
    }
    Ok(())
}

fn main() {
    let cli = Cli::parse();
    run::set_clipboard(cli.clipboard);
    run::set_no_mem_check(cli.no_mem_check);
    let r = match cli.cmd {
        Cmd::Create {
            name,
            size,
            desktop,
            ram,
            cpus,
            user,
            tarball,
            no_libvirt,
        } => (|| {
            create::create(create::CreateOpts {
                name: name.clone(),
                size: ui::parse_size(&size, 'G')?,
                desktop,
                ram_mib: ram_mib(&ram)?,
                cpus,
                user,
                tarball,
            })?;
            register(&name, no_libvirt)
        })(),
        Cmd::Import {
            path,
            name,
            mv,
            ram,
            cpus,
            user,
            kernel,
            share,
            net,
            no_libvirt,
        } => (|| {
            create::import(create::ImportOpts {
                path,
                name: name.clone(),
                mv,
                ram_mib: ram_mib(&ram)?,
                cpus,
                user,
                kernel,
                share,
                net_index: net,
            })?;
            register(&name, no_libvirt)
        })(),
        Cmd::StockKernel { name } => create::stock_kernel(&name),
        Cmd::List => list(),
        Cmd::Up {
            name,
            display,
            headless,
            vmm,
        } => opt_mode(display.as_deref()).and_then(|m| run::up(&name, m, headless, vmm)),
        Cmd::View {
            name,
            mode,
            tune_hyprland,
            fullscreen,
            vmm,
            keep_running,
        } => opt_mode(mode.as_deref())
            .and_then(|m| run::view(&name, m, tune_hyprland, fullscreen, vmm, keep_running)),
        Cmd::Down {
            name,
            timeout,
            force,
        } => run::down(
            &name,
            if force {
                run::Stop::Force
            } else {
                run::Stop::Soft(std::time::Duration::from_secs(timeout))
            },
        ),
        Cmd::Shutdown { name, timeout } => power::power(
            &name,
            power::Power::Shutdown(std::time::Duration::from_secs(timeout)),
        ),
        Cmd::Reboot { name } => power::power(&name, power::Power::Reboot),
        Cmd::Reset { name } => power::power(&name, power::Power::Reset),
        Cmd::Poweroff { name } => power::power(&name, power::Power::Poweroff),
        Cmd::Pause { name } => power::power(&name, power::Power::Pause),
        Cmd::Resume { name } => power::power(&name, power::Power::Resume),
        Cmd::Status { name } => run::status(name.as_deref()),
        Cmd::Logs {
            name,
            which,
            follow,
            lines,
        } => run::logs(&name, which.as_deref(), follow, lines),
        Cmd::Ssh {
            name,
            user,
            command,
        } => run::ssh(&name, user.as_deref(), &command),
        Cmd::Attach {
            name,
            dry_run,
            connect,
            guest_later,
        } => libvirt::attach(&name, dry_run, connect.as_deref(), guest_later),
        Cmd::Stream {
            target,
            arg,
            preset,
            port,
            display,
            service,
            stop,
            link,
            video_encryption,
            link_mbps,
            keep_vm,
        } => {
            if stream::ACTIONS.contains(&target.as_str()) {
                stream::action(&target, arg.as_slice())
            } else {
                opt_mode(display.as_deref()).and_then(|display| {
                    stream::stream(
                        &target,
                        &stream::Opts {
                            preset,
                            port,
                            display,
                            service,
                            stop,
                            link,
                            video_encryption,
                            link_mbps,
                        },
                        keep_vm,
                    )
                })
            }
        }
        Cmd::Trace {
            target,
            arg,
            follow,
            filter,
            summary,
            output,
            format,
            duration,
        } => match (target.as_str(), arg.as_deref()) {
            ("analyze", Some(file)) => trace::analyze(std::path::Path::new(file), &filter, follow),
            ("analyze", None) => Err(ui::oops(
                "usage: conduit trace analyze FILE",
                "FILE is a trace from `conduit trace NAME` or the backend's --trace",
            )),
            (name, Some(a @ ("status" | "on" | "off"))) => trace::action(name, a),
            (_, Some(other)) => Err(ui::oops(
                format!("unknown trace action {other:?}"),
                "Use: conduit trace NAME [status|on|off] or conduit trace analyze FILE",
            )),
            (name, None) => trace::live(
                name,
                trace::Opts {
                    follow,
                    summary,
                    output,
                    format,
                    filter,
                    duration,
                },
            ),
        },
        Cmd::Remote {
            host,
            token,
            lossless,
            codec,
            bitrate,
            fps,
            yuv444,
            fullscreen,
        } => stream::remote(&stream::RemoteOpts {
            host,
            token,
            lossless,
            codec,
            bitrate,
            fps,
            fullscreen,
            yuv444,
        }),
        Cmd::Detach { name } => libvirt::detach(&name),
        Cmd::Libvirt { action, name } => match action.as_str() {
            "enable" => virt::enable(&name),
            _ => virt::disable(&name),
        },
        Cmd::Config { action, key, value } => match (action.as_str(), key, value) {
            ("get", k, None) => config::get(k.as_deref()),
            ("set", Some(k), Some(v)) => config::set(&k, &v),
            ("unset", Some(k), None) => config::unset(&k),
            _ => Err(ui::oops(
                "usage: conduit config get [KEY] | set KEY VALUE | unset KEY",
                "e.g. conduit config set view.close_stops_vm false",
            )),
        },
        Cmd::Doctor { name: None } => std::process::exit(doctor::run()),
        Cmd::Doctor { name: Some(n) } => std::process::exit(doctor::run_vm(&n)),
        Cmd::Backend { name } => lvrun::backend_exec(&name),
        Cmd::Virtiofsd { name } => lvrun::virtiofsd_exec(&name),
        Cmd::Stopped { name } => lvrun::stopped(&name),
        Cmd::HyprHook { action, state } => hypr::hook(&action, &state),
        Cmd::Watch { name } => run::watch(&name),
    };
    if let Err(e) = r {
        ui::report(&e);
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn readme_commands_parse() {
        for args in [
            vec!["conduit", "create", "myvm"],
            vec![
                "conduit",
                "create",
                "myvm",
                "--size",
                "64G",
                "--desktop",
                "xfce",
                "--ram",
                "8G",
                "--cpus",
                "4",
            ],
            vec!["conduit", "view", "myvm"],
            vec!["conduit", "view", "myvm", "--clipboard", "to-guest"],
            vec!["conduit", "view", "myvm", "--clipboard", "off"],
            vec![
                "conduit",
                "view",
                "myvm",
                "2560x1440@240",
                "--tune-hyprland",
            ],
            vec!["conduit", "attach", "myvm"],
            vec!["conduit", "up", "myvm"],
            vec!["conduit", "up", "myvm", "--vmm", "qemu", "--headless"],
            vec!["conduit", "view", "myvm", "--vmm", "builtin"],
            vec!["conduit", "up", "myvm", "--no-mem-check"],
            vec!["conduit", "down", "myvm"],
            vec!["conduit", "status"],
            vec!["conduit", "status", "myvm"],
            vec!["conduit", "logs", "myvm"],
            vec!["conduit", "logs", "myvm", "backend", "-f"],
            vec!["conduit", "ssh", "myvm", "nvidia-smi", "-L"],
            vec!["conduit", "import", "/x/rootfs.ext4", "lab", "--move"],
            vec!["conduit", "list"],
            vec!["conduit", "stock-kernel", "lab"],
            vec!["conduit", "doctor"],
            vec!["conduit", "stream", "myvm"],
            vec!["conduit", "stream", "myvm", "--service"],
            vec!["conduit", "stream", "myvm", "--stop"],
            vec!["conduit", "stream", "pair", "1234"],
            vec!["conduit", "stream", "clients"],
            vec!["conduit", "remote", "box", "--lossless"],
            vec!["conduit", "doctor", "myvm"],
            vec![
                "conduit",
                "attach",
                "myvm",
                "-c",
                "qemu:///system",
                "--guest-later",
            ],
            vec!["conduit", "detach", "myvm"],
            vec!["conduit", "down", "myvm", "--timeout", "10"],
            vec!["conduit", "down", "myvm", "--force"],
            vec!["conduit", "shutdown", "myvm", "--timeout", "90"],
            vec!["conduit", "reboot", "myvm"],
            vec!["conduit", "reset", "myvm"],
            vec!["conduit", "poweroff", "myvm"],
            vec!["conduit", "pause", "myvm"],
            vec!["conduit", "resume", "myvm"],
            vec!["conduit", "libvirt", "enable", "myvm"],
            vec!["conduit", "libvirt", "disable", "myvm"],
            vec!["conduit", "view", "myvm", "--keep-running"],
            vec!["conduit", "config", "set", "view.close_stops_vm", "false"],
            vec!["conduit", "config", "get"],
            vec!["conduit", "create", "myvm", "--no-libvirt"],
        ] {
            Cli::try_parse_from(&args).unwrap_or_else(|e| panic!("{args:?}: {e}"));
        }
        assert!(Cli::try_parse_from(["conduit", "create", "x", "--desktop", "kde"]).is_err());
        assert!(Cli::try_parse_from([
            "conduit",
            "up",
            "x",
            "--headless",
            "--display",
            "1920x1080"
        ])
        .is_err());
    }

    #[test]
    fn memory_sizes() {
        assert_eq!(ram_mib("8G").unwrap(), 8192);
        assert_eq!(ram_mib("4096M").unwrap(), 4096);
        assert_eq!(ram_mib("16").unwrap(), 16384);
        assert!(ram_mib("100M").is_err());
    }
}
