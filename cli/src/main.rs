//! conduit: share your NVIDIA GPU with a Linux VM and see its desktop in a window.

mod create;
mod doctor;
mod host;
mod hypr;
mod libvirt;
mod mem;
mod mode;
mod net;
mod paths;
mod qemu;
mod run;
mod scope;
mod sys;
mod ui;
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
        /// Guest kernel to boot (if the disk's driver was built for a specific one)
        #[arg(long, value_name = "VMLINUX")]
        kernel: Option<PathBuf>,
        /// Share this NVIDIA user-space folder instead of staging the host's
        #[arg(long, value_name = "DIR")]
        share: Option<PathBuf>,
        /// VM network number N: the VM must use 172.30.N.2 (default: first free)
        #[arg(long, value_name = "N")]
        net: Option<u8>,
    },
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
    /// Open a VM in a window, starting it if needed. Closing the window shuts it down.
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
    },
    /// Shut a VM down cleanly and stop everything that belongs to it
    Down { name: String },
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
    /// Add the Conduit GPU to an existing libvirt / virt-manager VM (QEMU 11.1+)
    Attach {
        name: String,
        /// Only print the changed VM definition
        #[arg(long)]
        dry_run: bool,
        /// libvirt connection, e.g. qemu:///system
        #[arg(long, short = 'c')]
        connect: Option<String>,
    },
    /// Check this computer and explain how to fix problems
    Doctor,
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
    let names = vm::all();
    if names.is_empty() {
        println!("No VMs yet. Create one with `conduit create myvm`.");
        return Ok(());
    }
    println!(
        "{:<20} {:<9} {:>9} {:>8} {:>5}  {:<8}  ADDRESS",
        "NAME", "STATE", "DISK", "RAM", "CPUS", "DESKTOP"
    );
    for n in names {
        match vm::VmConfig::load(&n) {
            Ok(c) => {
                let disk = std::fs::metadata(c.disk_path())
                    .map(|m| ui::human_bytes(m.blocks() * 512))
                    .unwrap_or_else(|_| "missing".into());
                let state = if run::is_running(&n) {
                    "running"
                } else {
                    "stopped"
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
        } => (|| {
            create::create(create::CreateOpts {
                name,
                size: ui::parse_size(&size, 'G')?,
                desktop,
                ram_mib: ram_mib(&ram)?,
                cpus,
                user,
                tarball,
            })
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
        } => (|| {
            create::import(create::ImportOpts {
                path,
                name,
                mv,
                ram_mib: ram_mib(&ram)?,
                cpus,
                user,
                kernel,
                share,
                net_index: net,
            })
        })(),
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
        } => opt_mode(mode.as_deref())
            .and_then(|m| run::view(&name, m, tune_hyprland, fullscreen, vmm)),
        Cmd::Down { name } => run::down(&name),
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
        } => libvirt::attach(&name, dry_run, connect.as_deref()),
        Cmd::Doctor => std::process::exit(doctor::run()),
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
            vec!["conduit", "doctor"],
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
