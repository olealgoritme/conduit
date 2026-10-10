//! systemd units that tie a VM's helpers to its libvirt domain.
//!
//! libvirt runs QEMU; Conduit's helpers (the GPU backend, virtiofsd for the
//! NVIDIA share) are socket-activated around it. `conduit-backend@NAME.socket`
//! and `conduit-virtiofsd@NAME.socket` listen all the time; QEMU connecting to
//! them at start (virt-manager "Run", `virsh start`, `conduit up`) makes
//! systemd start the helper with the listening socket; the helper serves that
//! one connection and exits when QEMU goes away. No libvirt hook is needed,
//! which matters because the session daemon (qemu:///session) has none.
//!
//! Session domains get user units (~/.config/systemd/user). Domains of the
//! system daemon (qemu:///system, `conduit attach -c qemu:///system`) get
//! system units in /etc/systemd/system whose sockets belong to the user QEMU
//! runs as (libvirt-qemu / qemu), while the helpers still run as you.
//!
//! A VM's private network (tap `conduitN` + NAT) is one root oneshot unit,
//! `conduit-net-NAME.service`, enabled at boot: libvirt's session daemon cannot
//! create taps, so the tap must already exist when the domain starts.

use crate::paths;
use crate::scope;
use crate::sys;
use crate::ui::oops;
use crate::vm::VmConfig;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Which systemd manager the units live in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Scope {
    /// The user's own manager: for qemu:///session.
    User,
    /// The system manager: for qemu:///system. Sockets owned by `qemu_user`,
    /// helpers run as `user` (uid, home).
    System {
        user: String,
        uid: u32,
        home: PathBuf,
        qemu_user: String,
    },
}

/// The two helpers.
pub const HELPERS: [&str; 2] = ["backend", "virtiofsd"];

pub fn unit(helper: &str, vm: &str, kind: &str) -> String {
    format!("conduit-{helper}@{vm}.{kind}")
}

/// Where a helper listens (what the domain's QEMU connects to).
pub fn socket_path(scope: &Scope, vm: &str, helper: &str) -> PathBuf {
    match scope {
        Scope::User => paths::run_dir(vm).join(format!("{}-libvirt.sock", short(helper))),
        Scope::System { .. } => PathBuf::from(format!("/run/conduit/{vm}/{}.sock", short(helper))),
    }
}

/// Where the domain's QEMU listens on the stats channel
/// (`org.conduit.stats.0`, a virtio-serial port); `conduit _stats` connects
/// to it. QEMU creates the socket, so the folder must be one it can write to:
/// the VM's runtime folder for a session domain, the console folder (see
/// [`console_tmpfiles`]) for a system one.
pub fn stats_path(scope: &Scope, vm: &str) -> PathBuf {
    match scope {
        Scope::User => paths::run_dir(vm).join("stats.sock"),
        Scope::System { .. } => PathBuf::from(format!("/run/conduit/{vm}/console/stats.sock")),
    }
}

/// `conduit-stats@NAME.service`: feeds the VM's stats channel.
pub fn stats_unit(vm: &str) -> String {
    format!("conduit-stats@{vm}.service")
}

/// The template unit for the stats feed. It is not socket-activated (QEMU,
/// not the helper, owns the socket): it runs from login/boot and waits for the
/// VM's socket to appear.
pub fn stats_template(scope: &Scope, conduit: &Path) -> String {
    let target = match scope {
        Scope::User => "default.target",
        Scope::System { .. } => "multi-user.target",
    };
    format!(
        "# Installed by `conduit` (libvirt integration). Regenerated on `conduit libvirt enable`/`attach`.\n\
         [Unit]\n\
         Description=Conduit GPU stats feed for VM %i\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exe} _stats %i\n\
         Restart=always\n\
         RestartSec=5\n\
         OOMScoreAdjust={oom}\n\
         \n\
         [Install]\n\
         WantedBy={target}\n",
        exe = conduit.display(),
        oom = scope::OOM_SCORE_ADJ,
    )
}

/// Where the domain's QEMU listens with its VNC server (the boot console:
/// firmware, boot menu, disk-unlock prompt). QEMU creates this socket and the
/// backend connects to it (`--console-vnc`), the other way round from the
/// helper sockets. Session: the VM's runtime folder (QEMU runs as you).
/// System: QEMU runs as `qemu_user` and cannot create files in the
/// root-owned `/run/conduit/VM`, so it gets a subfolder of its own (see
/// [`console_tmpfiles`]).
pub fn console_path(scope: &Scope, vm: &str) -> PathBuf {
    match scope {
        Scope::User => paths::run_dir(vm).join("console.sock"),
        Scope::System { .. } => PathBuf::from(format!("/run/conduit/{vm}/console/vnc.sock")),
    }
}

/// System domains: tmpfiles.d entry for the console socket's folder, owned by
/// the QEMU user, group `group` (yours), setgid. libvirt starts QEMU with
/// umask 002, so the socket it creates there is group-writable and in your
/// group: the backend (running as you) can connect, nobody else can.
pub fn console_tmpfiles(qemu_user: &str, group: &str, vm: &str) -> String {
    format!(
        "# Installed by `conduit attach` for VM {vm}: the boot console's VNC socket folder.\n\
         d /run/conduit/{vm}/console 2750 {qemu_user} {group} -\n"
    )
}

const TMPFILES_DIR: &str = "/etc/tmpfiles.d";

fn tmpfiles_conf(vm: &str) -> String {
    format!("{TMPFILES_DIR}/conduit-{vm}.conf")
}

fn short(helper: &str) -> &str {
    if helper == "backend" {
        "gpu"
    } else {
        "vfs"
    }
}

fn helper_desc(helper: &str) -> &'static str {
    if helper == "backend" {
        "GPU backend"
    } else {
        "NVIDIA share (virtiofsd)"
    }
}

/// The template .socket for a helper.
pub fn socket_template(scope: &Scope, helper: &str) -> String {
    let (listen, owner) = match scope {
        Scope::User => (
            format!("%t/conduit/%i/{}-libvirt.sock", short(helper)),
            String::new(),
        ),
        Scope::System { .. } => (
            format!("/run/conduit/%i/{}.sock", short(helper)),
            // SocketUser= comes from the per-VM drop-in.
            String::new(),
        ),
    };
    format!(
        "# Installed by `conduit` (libvirt integration). Regenerated on `conduit libvirt enable`/`attach`.\n\
         [Unit]\n\
         Description=Conduit {desc} socket for VM %i\n\
         \n\
         [Socket]\n\
         ListenStream={listen}\n\
         SocketMode=0600\n\
         DirectoryMode=0700\n\
         Accept=no\n\
         RemoveOnStop=yes\n{owner}\
         \n\
         [Install]\n\
         WantedBy=sockets.target\n",
        desc = helper_desc(helper),
    )
}

/// The template .service: `conduit _HELPER NAME` resolves everything at start
/// (display mode, driver share, logs) and then execs the helper in place.
pub fn service_template(helper: &str, conduit: &Path) -> String {
    let stop_post = if helper == "backend" {
        format!(
            "# After the VM: refresh its boot files (a kernel updated inside it boots next time).\nExecStopPost=-{} _stopped %i\n",
            conduit.display()
        )
    } else {
        String::new()
    };
    format!(
        "# Installed by `conduit` (libvirt integration). Regenerated on `conduit libvirt enable`/`attach`.\n\
         [Unit]\n\
         Description=Conduit {desc} for VM %i\n\
         Requires=conduit-{helper}@%i.socket\n\
         After=conduit-{helper}@%i.socket\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exe} _{helper} %i\n\
         {stop_post}\
         # It serves one VM run, then exits; the socket starts it again next time.\n\
         Restart=no\n\
         # Under memory pressure the kernel kills a VM's helpers before the desktop.\n\
         OOMScoreAdjust={oom}\n\
         TimeoutStopSec=20\n",
        desc = helper_desc(helper),
        exe = conduit.display(),
        oom = scope::OOM_SCORE_ADJ,
    )
}

/// Per-VM drop-in: the VM's slice (and, for system units, who runs it).
pub fn service_dropin(scope: &Scope, vm: &str) -> String {
    let mut s = format!(
        "# Installed by `conduit` for VM {vm}.\n[Service]\nSlice={}\n",
        scope::slice_name(vm)
    );
    if let Scope::System {
        user, uid, home, ..
    } = scope
    {
        s += &format!(
            "User={user}\nEnvironment=HOME={home} XDG_RUNTIME_DIR=/run/user/{uid} USER={user}\n",
            home = home.display()
        );
    }
    s
}

pub fn socket_dropin(scope: &Scope, vm: &str) -> Option<String> {
    match scope {
        Scope::User => None,
        Scope::System { qemu_user, .. } => Some(format!(
            "# Installed by `conduit` for VM {vm}: QEMU runs as {qemu_user}.\n[Socket]\nSocketUser={qemu_user}\nDirectoryMode=0755\n"
        )),
    }
}

// ---------------------------------------------------------------- shared folders

/// `conduit-share@VM:SHARE.KIND`: one virtiofsd per shared folder.
pub fn share_unit(vm: &str, share: &str, kind: &str) -> String {
    format!("conduit-share@{vm}:{share}.{kind}")
}

/// Where a shared folder's virtiofsd listens (what QEMU connects to).
pub fn share_socket_path(scope: &Scope, vm: &str, share: &str) -> PathBuf {
    match scope {
        Scope::User => paths::run_dir(vm).join(format!("share-{share}.sock")),
        Scope::System { .. } => PathBuf::from(format!("/run/conduit/{vm}/share-{share}.sock")),
    }
}

/// The template .socket of a shared folder; the listening path comes from the
/// per-share drop-in ([`share_socket_dropin`]), since a template cannot split
/// its `VM:SHARE` instance name.
pub fn share_socket_template() -> String {
    "# Installed by `conduit` (shared folders). Regenerated on `conduit attach`/`share`.\n\
     [Unit]\n\
     Description=Conduit shared folder socket %i\n\
     \n\
     [Socket]\n\
     SocketMode=0600\n\
     DirectoryMode=0700\n\
     Accept=no\n\
     RemoveOnStop=yes\n\
     \n\
     [Install]\n\
     WantedBy=sockets.target\n"
        .into()
}

/// The template .service: `conduit _share VM:SHARE` execs virtiofsd on the socket.
pub fn share_service_template(conduit: &Path) -> String {
    format!(
        "# Installed by `conduit` (shared folders). Regenerated on `conduit attach`/`share`.\n\
         [Unit]\n\
         Description=Conduit shared folder %i (virtiofsd)\n\
         Requires=conduit-share@%i.socket\n\
         After=conduit-share@%i.socket\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exe} _share %i\n\
         # It serves one VM run, then exits; the socket starts it again next time.\n\
         Restart=no\n\
         OOMScoreAdjust={oom}\n\
         TimeoutStopSec=20\n",
        exe = conduit.display(),
        oom = scope::OOM_SCORE_ADJ,
    )
}

/// Per-share socket drop-in: where it listens (and, for system units, who owns it).
pub fn share_socket_dropin(scope: &Scope, vm: &str, share: &str) -> String {
    let mut s = format!(
        "# Installed by `conduit` for VM {vm}, shared folder {share}.\n[Socket]\nListenStream={}\n",
        share_socket_path(scope, vm, share).display()
    );
    if let Scope::System { qemu_user, .. } = scope {
        s += &format!("SocketUser={qemu_user}\nDirectoryMode=0755\n");
    }
    s
}

/// Shared folders with units installed for this VM (read from the drop-in folders).
pub fn installed_shares(scope: &Scope, vm: &str) -> Vec<String> {
    let dir = match scope {
        Scope::User => user_unit_dir(),
        Scope::System { .. } => PathBuf::from(SYSTEM_UNIT_DIR),
    };
    let prefix = format!("conduit-share@{vm}:");
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.strip_prefix(&prefix)?
                .strip_suffix(".socket.d")
                .map(String::from)
        })
        .collect();
    v.sort();
    v
}

/// Install the units of these shared folders and start their sockets. The
/// shares in `keep` are the configured ones; installed ones not in it are
/// removed unless `busy` says QEMU may still use them.
pub fn install_shares(
    scope: &Scope,
    vm: &str,
    conduit: &Path,
    keep: &[String],
    busy: &dyn Fn(&str) -> bool,
) -> Result<()> {
    put(scope, "conduit-share@.socket", &share_socket_template())?;
    put(
        scope,
        "conduit-share@.service",
        &share_service_template(conduit),
    )?;
    for sh in keep {
        put(
            scope,
            &format!("{}.d/conduit.conf", share_unit(vm, sh, "socket")),
            &share_socket_dropin(scope, vm, sh),
        )?;
        put(
            scope,
            &format!("{}.d/conduit.conf", share_unit(vm, sh, "service")),
            &service_dropin(scope, vm),
        )?;
    }
    for sh in installed_shares(scope, vm) {
        if !keep.contains(&sh) && !busy(&sh) {
            remove_share(scope, vm, &sh);
        }
    }
    systemctl(scope, &["daemon-reload"])?;
    for sh in keep {
        let u = share_unit(vm, sh, "socket");
        let _ = systemctl(scope, &["reset-failed", &u]);
        systemctl(scope, &["enable", "--now", &u])
            .with_context(|| format!("could not start the socket of shared folder {sh}"))?;
    }
    Ok(())
}

/// Stop and forget one shared folder's units.
pub fn remove_share(scope: &Scope, vm: &str, share: &str) {
    let _ = systemctl(
        scope,
        &["disable", "--now", &share_unit(vm, share, "socket")],
    );
    let _ = systemctl(scope, &["stop", &share_unit(vm, share, "service")]);
    rm(
        scope,
        &format!("{}.d/conduit.conf", share_unit(vm, share, "socket")),
    );
    rm(
        scope,
        &format!("{}.d/conduit.conf", share_unit(vm, share, "service")),
    );
}

fn user_unit_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| paths::home().join(".config"))
        .join("systemd/user")
}

const SYSTEM_UNIT_DIR: &str = "/etc/systemd/system";

fn systemctl(scope: &Scope, args: &[&str]) -> Result<()> {
    match scope {
        Scope::User => {
            let mut a = vec!["--user"];
            a.extend_from_slice(args);
            sys::output("systemctl", &a).map(|_| ())
        }
        Scope::System { .. } => sys::sudo("systemctl", args),
    }
}

fn systemctl_ok(scope: &Scope, args: &[&str]) -> bool {
    match scope {
        Scope::User => sys::quiet("systemctl", &[&["--user"], args].concat()),
        Scope::System { .. } => sys::quiet("systemctl", args),
    }
}

/// `systemctl is-active` of a VM's helper unit.
pub fn active(scope: &Scope, vm: &str, helper: &str, kind: &str) -> bool {
    systemctl_ok(scope, &["is-active", "--quiet", &unit(helper, vm, kind)])
}

/// Is this a systemd we can use? (user manager reachable for User.)
pub fn check(scope: &Scope) -> Result<()> {
    let ok = match scope {
        Scope::User => {
            sys::quiet("systemctl", &["--user", "is-system-running"])
                || sys::output("systemctl", &["--user", "show", "-p", "Version"]).is_ok()
        }
        Scope::System { .. } => sys::have("systemctl"),
    };
    if ok {
        Ok(())
    } else {
        Err(oops(
            "no systemd user session: Conduit starts the VM's GPU backend through it",
            "Log in to a normal desktop session (systemd --user must be running), then try again",
        ))
    }
}

/// Write a file under `dir` (staged first for system units, then sudo-installed).
fn put(scope: &Scope, rel: &str, body: &str) -> Result<()> {
    match scope {
        Scope::User => {
            let p = user_unit_dir().join(rel);
            std::fs::create_dir_all(p.parent().unwrap())?;
            std::fs::write(&p, body).with_context(|| format!("writing {}", p.display()))
        }
        Scope::System { .. } => {
            let stage = paths::config_dir().join("staged-units");
            std::fs::create_dir_all(&stage)?;
            let tmp = stage.join(rel.replace('/', "_"));
            std::fs::write(&tmp, body)?;
            let dst = Path::new(SYSTEM_UNIT_DIR).join(rel);
            sys::sudo(
                "install",
                &["-D", "-m644", tmp.to_str().unwrap(), dst.to_str().unwrap()],
            )?;
            let _ = std::fs::remove_file(tmp);
            Ok(())
        }
    }
}

fn rm(scope: &Scope, rel: &str) {
    match scope {
        Scope::User => {
            let p = user_unit_dir().join(rel);
            let _ = std::fs::remove_file(&p);
            if let Some(d) = p.parent() {
                let _ = std::fs::remove_dir(d); // only when empty
            }
        }
        Scope::System { .. } => {
            let p = Path::new(SYSTEM_UNIT_DIR).join(rel);
            let _ = sys::sudo("rm", &["-f", p.to_str().unwrap()]);
            if let Some(d) = p
                .parent()
                .filter(|d| d.extension().is_some_and(|e| e == "d"))
            {
                let _ = sys::sudo(
                    "rmdir",
                    &["--ignore-fail-on-non-empty", d.to_str().unwrap()],
                );
            }
        }
    }
}

/// Install the templates and this VM's drop-ins, and start listening.
pub fn install(scope: &Scope, vm: &str, conduit: &Path) -> Result<()> {
    if matches!(scope, Scope::System { .. }) {
        sys::sudo_ready("Installing system units that start Conduit's GPU backend with the VM.")?;
    }
    for h in HELPERS {
        put(
            scope,
            &format!("conduit-{h}@.socket"),
            &socket_template(scope, h),
        )?;
        put(
            scope,
            &format!("conduit-{h}@.service"),
            &service_template(h, conduit),
        )?;
        put(
            scope,
            &format!("{}.d/conduit.conf", unit(h, vm, "service")),
            &service_dropin(scope, vm),
        )?;
        if let Some(d) = socket_dropin(scope, vm) {
            put(
                scope,
                &format!("{}.d/conduit.conf", unit(h, vm, "socket")),
                &d,
            )?;
        }
    }
    put(
        scope,
        "conduit-stats@.service",
        &stats_template(scope, conduit),
    )?;
    put(
        scope,
        &format!("{}.d/conduit.conf", stats_unit(vm)),
        &service_dropin(scope, vm),
    )?;
    if let Scope::System {
        user, qemu_user, ..
    } = scope
    {
        let group = sys::output("id", &["-gn", user])
            .map(|g| g.trim().to_string())
            .context("cannot tell your primary group")?;
        let stage = paths::config_dir().join("staged-units");
        std::fs::create_dir_all(&stage)?;
        let tmp = stage.join(format!("conduit-{vm}.conf"));
        std::fs::write(&tmp, console_tmpfiles(qemu_user, &group, vm))?;
        let dst = tmpfiles_conf(vm);
        sys::sudo("install", &["-D", "-m644", tmp.to_str().unwrap(), &dst])?;
        let _ = std::fs::remove_file(tmp);
        sys::sudo("systemd-tmpfiles", &["--create", &dst])
            .context("could not create the boot console's socket folder")?;
    }
    systemctl(scope, &["daemon-reload"])?;
    let socks: Vec<String> = HELPERS.iter().map(|h| unit(h, vm, "socket")).collect();
    let mut a = vec!["enable", "--now"];
    a.extend(socks.iter().map(String::as_str));
    systemctl(scope, &a).context("could not start the VM's helper sockets")?;
    // The stats feed is best effort: the VM works without it.
    if let Err(e) = systemctl(scope, &["enable", "--now", &stats_unit(vm)]) {
        eprintln!("conduit: could not start the GPU stats feed: {e:#}");
    }
    // Shared folders: the default one on first attach, then every configured one.
    let names: Vec<String> = crate::shares::load_or_init(vm)?
        .into_iter()
        .map(|s| s.name)
        .collect();
    install_shares(scope, vm, conduit, &names, &|_| false)?;
    Ok(())
}

/// Stop listening and remove this VM's drop-ins (the templates stay: shared).
pub fn remove(scope: &Scope, vm: &str) {
    let socks: Vec<String> = HELPERS.iter().map(|h| unit(h, vm, "socket")).collect();
    let mut a = vec!["disable", "--now"];
    a.extend(socks.iter().map(String::as_str));
    let _ = systemctl(scope, &a);
    let _ = systemctl(scope, &["disable", "--now", &stats_unit(vm)]);
    rm(scope, &format!("{}.d/conduit.conf", stats_unit(vm)));
    for sh in installed_shares(scope, vm) {
        remove_share(scope, vm, &sh);
    }
    for h in HELPERS {
        let _ = systemctl(scope, &["stop", &unit(h, vm, "service")]);
        rm(scope, &format!("{}.d/conduit.conf", unit(h, vm, "service")));
        rm(scope, &format!("{}.d/conduit.conf", unit(h, vm, "socket")));
    }
    let _ = systemctl(scope, &["daemon-reload"]);
    if matches!(scope, Scope::System { .. }) {
        let _ = sys::sudo("rm", &["-f", &tmpfiles_conf(vm)]);
        let _ = sys::sudo("rm", &["-rf", &format!("/run/conduit/{vm}/console")]);
    }
    // The helpers ran in the VM's slice; drop it too.
    let _ = systemctl(scope, &["stop", &scope::slice_name(vm)]);
}

/// Listening, or made to listen again: a helper that failed once (e.g. the
/// share could not be staged) leaves its socket failed until reset.
pub fn ensure_listening(scope: &Scope, vm: &str) -> bool {
    // Shared folders are best effort: the VM starts without them.
    for sh in installed_shares(scope, vm) {
        let u = share_unit(vm, &sh, "socket");
        if !systemctl_ok(scope, &["is-active", "--quiet", &u]) {
            let _ = systemctl(scope, &["reset-failed", &u]);
            let _ = systemctl(scope, &["start", &u]);
        }
    }
    if listening(scope, vm) {
        return true;
    }
    for h in HELPERS {
        for kind in ["service", "socket"] {
            let _ = systemctl(scope, &["reset-failed", &unit(h, vm, kind)]);
        }
        let _ = systemctl(scope, &["start", &unit(h, vm, "socket")]);
    }
    listening(scope, vm)
}

/// Are the sockets listening (the VM can start)?
pub fn listening(scope: &Scope, vm: &str) -> bool {
    HELPERS.iter().all(|h| active(scope, vm, h, "socket"))
}

// ---------------------------------------------------------------- network

pub fn net_unit(vm: &str) -> String {
    format!("conduit-net-{vm}.service")
}

/// The root oneshot unit that keeps a VM's tap and NAT in place from boot.
pub fn net_unit_body(c: &VmConfig, user: &str, ip: &str, iptables: &str, sysctl: &str) -> String {
    let n = c.net();
    let rules = crate::net::rules(c);
    let fmt = |r: &[String], op: &str| {
        let v = crate::net::with_op(r, op);
        format!("{iptables} {}", v.join(" "))
    };
    let mut up = vec![
        format!("{ip} link show {tap} >/dev/null 2>&1 || {ip} tuntap add dev {tap} mode tap user {user}", tap = n.tap),
        format!("{ip} addr replace {}/24 dev {}", n.host_ip, n.tap),
        format!("{ip} link set {} up", n.tap),
        format!("{sysctl} -qw net.ipv4.ip_forward=1"),
    ];
    for (i, r) in rules.iter().enumerate() {
        let op = if i == 0 { "-A" } else { "-I" };
        up.push(format!(
            "{{ {} 2>/dev/null || {}; }}",
            fmt(r, "-C"),
            fmt(r, op)
        ));
    }
    let mut down: Vec<String> = rules
        .iter()
        .map(|r| format!("while {} 2>/dev/null; do :; done", fmt(r, "-D")))
        .collect();
    down.push(format!("{ip} link del {} 2>/dev/null || true", n.tap));
    format!(
        "# Installed by `conduit libvirt enable {vm}`: VM {vm}'s private network\n\
         # (tap {tap} owned by {user}, host {host}, VM {guest}, NAT). libvirt's session\n\
         # daemon cannot create taps, so this one exists from boot.\n\
         [Unit]\n\
         Description=Conduit network for VM {vm} ({tap}, {subnet})\n\
         After=network-pre.target\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         RemainAfterExit=yes\n\
         ExecStart=/bin/sh -c '{up}'\n\
         ExecStop=/bin/sh -c '{down}'\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n",
        vm = c.name,
        tap = n.tap,
        host = n.host_ip,
        guest = n.guest_ip,
        subnet = n.subnet,
        up = up.join("; "),
        down = down.join("; "),
    )
}

pub fn install_net(c: &VmConfig) -> Result<()> {
    let need = |t: &str| {
        sys::which(t).ok_or_else(|| {
            oops(
                format!("`{t}` is not installed (needed for the VM's network)"),
                "Install iproute2, iptables and procps",
            )
        })
    };
    let (ip, ipt, sysctl) = (need("ip")?, need("iptables")?, need("sysctl")?);
    let body = net_unit_body(
        c,
        &paths::username(),
        &ip.to_string_lossy(),
        &ipt.to_string_lossy(),
        &sysctl.to_string_lossy(),
    );
    let unit_name = net_unit(&c.name);
    let path = Path::new(SYSTEM_UNIT_DIR).join(&unit_name);
    if std::fs::read_to_string(&path).ok().as_deref() == Some(body.as_str())
        && sys::quiet("systemctl", &["is-active", "--quiet", &unit_name])
    {
        return Ok(());
    }
    sys::sudo_ready(&format!(
        "Installing {unit_name}, which keeps the VM's network device ({}) ready from boot.",
        c.net().tap
    ))?;
    let sys_scope = Scope::System {
        user: String::new(),
        uid: 0,
        home: PathBuf::new(),
        qemu_user: String::new(),
    };
    put(&sys_scope, &unit_name, &body)?;
    sys::sudo("systemctl", &["daemon-reload"])?;
    sys::sudo("systemctl", &["enable", "--now", &unit_name])
        .context("could not set up the VM's network")?;
    Ok(())
}

pub fn net_installed(vm: &str) -> bool {
    Path::new(SYSTEM_UNIT_DIR).join(net_unit(vm)).is_file()
}

pub fn remove_net(vm: &str) -> Result<()> {
    if !net_installed(vm) {
        return Ok(());
    }
    sys::sudo_ready("Removing the VM's network unit.")?;
    let unit_name = net_unit(vm);
    let _ = sys::sudo("systemctl", &["disable", "--now", &unit_name]);
    sys::sudo("rm", &["-f", &format!("{SYSTEM_UNIT_DIR}/{unit_name}")])?;
    let _ = sys::sudo("systemctl", &["daemon-reload"]);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_feed_unit() {
        let v = stats_template(&Scope::User, Path::new("/usr/bin/conduit"));
        assert!(v.contains("ExecStart=/usr/bin/conduit _stats %i"));
        assert!(v.contains("WantedBy=default.target"));
        assert!(stats_path(&Scope::User, "w").ends_with("conduit/w/stats.sock"));
    }

    #[test]
    fn user_socket_lives_in_the_runtime_dir() {
        let t = socket_template(&Scope::User, "backend");
        assert!(
            t.contains("ListenStream=%t/conduit/%i/gpu-libvirt.sock"),
            "{t}"
        );
        assert!(t.contains("SocketMode=0600"));
        assert!(t.contains("WantedBy=sockets.target"));
        let v = socket_template(&Scope::User, "virtiofsd");
        assert!(v.contains("%t/conduit/%i/vfs-libvirt.sock"));
    }

    #[test]
    fn service_execs_conduit_and_refreshes_boot_after() {
        let s = service_template("backend", Path::new("/usr/bin/conduit"));
        assert!(s.contains("ExecStart=/usr/bin/conduit _backend %i"), "{s}");
        assert!(s.contains("ExecStopPost=-/usr/bin/conduit _stopped %i"));
        assert!(s.contains("Restart=no"));
        assert!(s.contains("Requires=conduit-backend@%i.socket"));
        let v = service_template("virtiofsd", Path::new("/usr/bin/conduit"));
        assert!(v.contains("ExecStart=/usr/bin/conduit _virtiofsd %i"));
        assert!(!v.contains("ExecStopPost"));
    }

    #[test]
    fn dropins_name_the_slice_and_system_owner() {
        let d = service_dropin(&Scope::User, "my-vm");
        assert!(d.contains("Slice=conduit-my\\x2dvm.slice"), "{d}");
        let sys = Scope::System {
            user: "ana".into(),
            uid: 1000,
            home: "/home/ana".into(),
            qemu_user: "libvirt-qemu".into(),
        };
        let d = service_dropin(&sys, "x");
        assert!(d.contains("User=ana"));
        assert!(d.contains("XDG_RUNTIME_DIR=/run/user/1000"));
        assert!(socket_dropin(&sys, "x")
            .unwrap()
            .contains("SocketUser=libvirt-qemu"));
        assert!(socket_dropin(&Scope::User, "x").is_none());
        assert_eq!(
            socket_path(&sys, "x", "backend"),
            PathBuf::from("/run/conduit/x/gpu.sock")
        );
    }

    #[test]
    fn console_socket_qemu_can_create_and_the_backend_reach() {
        let sys = Scope::System {
            user: "ana".into(),
            uid: 1000,
            home: "/home/ana".into(),
            qemu_user: "libvirt-qemu".into(),
        };
        assert_eq!(
            console_path(&sys, "x"),
            PathBuf::from("/run/conduit/x/console/vnc.sock")
        );
        assert!(console_path(&Scope::User, "x").ends_with("conduit/x/console.sock"));
        let t = console_tmpfiles("libvirt-qemu", "ana", "x");
        assert!(
            t.contains("d /run/conduit/x/console 2750 libvirt-qemu ana -"),
            "{t}"
        );
    }

    #[test]
    fn share_units_listen_per_share() {
        let d = share_socket_dropin(&Scope::User, "vm1", "Docs");
        assert!(d.contains("ListenStream=") && d.contains("vm1/share-Docs.sock"));
        let sys = Scope::System {
            user: "me".into(),
            uid: 1000,
            home: PathBuf::from("/home/me"),
            qemu_user: "libvirt-qemu".into(),
        };
        let d = share_socket_dropin(&sys, "vm1", "Docs");
        assert!(d.contains("ListenStream=/run/conduit/vm1/share-Docs.sock"));
        assert!(d.contains("SocketUser=libvirt-qemu"));
        let v = share_service_template(Path::new("/usr/bin/conduit"));
        assert!(v.contains("ExecStart=/usr/bin/conduit _share %i"));
        assert_eq!(
            share_unit("vm1", "Docs", "socket"),
            "conduit-share@vm1:Docs.socket"
        );
    }

    #[test]
    fn net_unit_is_idempotent_shell() {
        let c = VmConfig::new("t", 1024, 1, 7, "u", "none");
        let b = net_unit_body(
            &c,
            "ana",
            "/usr/sbin/ip",
            "/usr/sbin/iptables",
            "/usr/sbin/sysctl",
        );
        assert!(
            b.contains("tuntap add dev conduit7 mode tap user ana"),
            "{b}"
        );
        assert!(b.contains("addr replace 172.30.7.1/24 dev conduit7"));
        assert!(b.contains("-t nat -C POSTROUTING -s 172.30.7.0/24"));
        assert!(b.contains("-t nat -A POSTROUTING"));
        assert!(b.contains("-I FORWARD -i conduit7 -j ACCEPT"));
        assert!(b.contains("link del conduit7"));
        // One quoted sh -c argument per line: no stray single quotes inside.
        for l in b.lines().filter(|l| l.starts_with("Exec")) {
            assert_eq!(l.matches('\'').count(), 2, "{l}");
        }
    }
}
