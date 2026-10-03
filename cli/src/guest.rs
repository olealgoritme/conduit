//! Installing Conduit's guest side into a VM Conduit did not build
//! (`conduit attach`): the conduit-guest package (driver via DKMS, clipboard
//! agent) and the NVIDIA share setup that `conduit create` puts on its disks.
//!
//! Delivered through the QEMU guest agent when the VM runs one
//! (`virsh qemu-agent-command`: file upload, then guest-exec), otherwise as a
//! bundle under vms/NAME/guest-setup/ with one command to run inside the VM.

use crate::paths::{self, Tool};
use crate::sys;
use crate::ui::{self, oops};
use crate::virt::Link;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// Files `conduit create` installs for the share and the seat (cli/assets/guest).
const FILES: &[(&str, &str)] = &[
    (
        "conduit-guest.service",
        include_str!("../assets/guest/conduit-guest.service"),
    ),
    (
        "99-conduit.rules",
        include_str!("../assets/guest/99-conduit.rules"),
    ),
    (
        "71-conduit-seat.rules",
        include_str!("../assets/guest/71-conduit-seat.rules"),
    ),
    (
        "zz-conduit-nvidia.conf",
        include_str!("../assets/guest/zz-conduit-nvidia.conf"),
    ),
    (
        "conduit-nvidia.sh",
        include_str!("../assets/guest/conduit-nvidia.sh"),
    ),
    (
        "90-conduit-nvidia.conf",
        include_str!("../assets/guest/90-conduit-nvidia.conf"),
    ),
];

/// Runs inside the guest as root. Idempotent.
pub const SETUP: &str = r#"#!/bin/sh
# Conduit guest setup, from `conduit attach` on the host. Safe to run again.
set -eu
D=$(cd "$(dirname "$0")" && pwd)
if ! command -v apt-get >/dev/null 2>&1; then
    echo "conduit: this VM has no apt-get; install the conduit-guest package for your distribution by hand" >&2
    exit 3
fi
export DEBIAN_FRONTEND=noninteractive
apt-get update -q || true
# DKMS builds the driver for the running kernel: it needs that kernel's headers.
apt-get install -y -q --no-install-recommends "linux-headers-$(uname -r)" \
    || echo "conduit: warning: no headers package for $(uname -r); the driver builds once they are installed" >&2
apt-get install -y -q --no-install-recommends "$D/conduit-guest.deb"
install -d /mnt/nvidia /etc/environment.d
install -m644 "$D/conduit-guest.service" /etc/systemd/system/conduit-guest.service
install -m644 "$D/99-conduit.rules" /etc/udev/rules.d/99-conduit.rules
install -m644 "$D/71-conduit-seat.rules" /etc/udev/rules.d/71-conduit-seat.rules
install -m644 "$D/zz-conduit-nvidia.conf" /etc/ld.so.conf.d/zz-conduit-nvidia.conf
install -m644 "$D/conduit-nvidia.sh" /etc/profile.d/conduit-nvidia.sh
install -m644 "$D/90-conduit-nvidia.conf" /etc/environment.d/90-conduit-nvidia.conf
systemctl daemon-reload
systemctl enable conduit-guest.service
dkms status conduit-guest || true
echo "conduit: guest side installed. Restart the VM to use Conduit's GPU."
"#;

pub fn bundle_dir(name: &str) -> PathBuf {
    paths::vm_dir(name).join("guest-setup")
}

/// Write the bundle (setup.sh, the package, the files) and a tar of it.
pub fn bundle(name: &str) -> Result<PathBuf> {
    let deb = Tool::GuestDeb.require()?;
    let dir = bundle_dir(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    std::fs::copy(&deb, dir.join("conduit-guest.deb"))
        .with_context(|| format!("copying {}", deb.display()))?;
    std::fs::write(dir.join("setup.sh"), SETUP)?;
    for (f, body) in FILES {
        std::fs::write(dir.join(f), body)?;
    }
    let tar = paths::vm_dir(name).join("guest-setup.tar");
    let ok = Command::new("tar")
        .arg("-cf")
        .arg(&tar)
        .arg("-C")
        .arg(&dir)
        .arg(".")
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        bail!("could not pack the guest setup ({})", tar.display());
    }
    Ok(tar)
}

/// The one command that installs the bundle from the host over ssh.
pub fn manual_command(name: &str) -> String {
    let tar = paths::vm_dir(name).join("guest-setup.tar");
    format!(
        "ssh USER@VM-ADDRESS 'rm -rf /tmp/conduit-guest && mkdir -p /tmp/conduit-guest && tar -x -C /tmp/conduit-guest && sudo sh /tmp/conduit-guest/setup.sh' < {}",
        ui::shell_quote(&tar.to_string_lossy())
    )
}

// ---------------------------------------------------------------- guest agent

fn agent(link: &Link, cmd: Value, timeout_s: u32) -> Result<Value> {
    let out = link.virsh().run(&[
        "qemu-agent-command",
        &link.domain,
        &cmd.to_string(),
        "--timeout",
        &timeout_s.to_string(),
    ])?;
    let v: Value = serde_json::from_str(out.trim()).context("the guest agent's answer")?;
    Ok(v.get("return").cloned().unwrap_or(Value::Null))
}

/// Does the running VM answer on the QEMU guest agent channel?
pub fn agent_ready(link: &Link) -> bool {
    agent(link, json!({"execute": "guest-ping"}), 5).is_ok()
}

fn upload(link: &Link, src: &Path, dst: &str) -> Result<()> {
    use base64_lite::encode;
    let data = std::fs::read(src)?;
    let h = agent(
        link,
        json!({"execute":"guest-file-open","arguments":{"path":dst,"mode":"w"}}),
        10,
    )?;
    let h = h.as_i64().context("guest-file-open gave no handle")?;
    let r = (|| -> Result<()> {
        for chunk in data.chunks(48 * 1024) {
            agent(
                link,
                json!({"execute":"guest-file-write","arguments":{"handle":h,"buf-b64":encode(chunk)}}),
                20,
            )?;
        }
        Ok(())
    })();
    let _ = agent(
        link,
        json!({"execute":"guest-file-close","arguments":{"handle":h}}),
        10,
    );
    r
}

/// Run a shell command in the guest as root; (exit code, output).
pub fn agent_exec(link: &Link, script: &str, timeout: Duration) -> Result<(i64, String)> {
    let pid = agent(
        link,
        json!({"execute":"guest-exec","arguments":{"path":"/bin/sh","arg":["-c",script],"capture-output":true}}),
        10,
    )?;
    let pid = pid
        .get("pid")
        .and_then(Value::as_i64)
        .context("guest-exec gave no pid")?;
    let t = Instant::now();
    loop {
        let st = agent(
            link,
            json!({"execute":"guest-exec-status","arguments":{"pid":pid}}),
            10,
        )?;
        if st.get("exited").and_then(Value::as_bool) == Some(true) {
            let dec = |k: &str| {
                st.get(k)
                    .and_then(Value::as_str)
                    .and_then(|s| base64_lite::decode(s).ok())
                    .map(|b| String::from_utf8_lossy(&b).into_owned())
                    .unwrap_or_default()
            };
            let code = st.get("exitcode").and_then(Value::as_i64).unwrap_or(-1);
            return Ok((code, dec("out-data") + &dec("err-data")));
        }
        if t.elapsed() > timeout {
            bail!(
                "the guest command did not finish within {} s",
                timeout.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Install the guest side through the guest agent.
pub fn install_via_agent(name: &str, link: &Link) -> Result<()> {
    let tar = bundle(name)?;
    ui::info("copying the guest driver into the VM (QEMU guest agent)…");
    upload(link, &tar, "/tmp/conduit-guest-setup.tar")?;
    ui::info("installing it inside the VM (apt, DKMS build; this takes a minute or two)…");
    let (code, out) = agent_exec(
        link,
        "rm -rf /tmp/conduit-guest && mkdir -p /tmp/conduit-guest && tar -xf /tmp/conduit-guest-setup.tar -C /tmp/conduit-guest && sh /tmp/conduit-guest/setup.sh 2>&1",
        Duration::from_secs(900),
    )?;
    let log = paths::vm_dir(name).join("logs/guest-setup.log");
    let _ = std::fs::create_dir_all(log.parent().unwrap());
    let _ = std::fs::write(&log, &out);
    if code != 0 {
        return Err(oops(
            format!("installing the guest side inside {name} failed (exit {code})"),
            format!("The VM's output: {}\n{}", log.display(), sys::tail(&log, 6)),
        ));
    }
    Ok(())
}

/// The guest driver version inside a running VM, through the agent.
pub fn driver_version_via_agent(link: &Link) -> Option<String> {
    let (code, out) = agent_exec(link, VERSION_PROBE, Duration::from_secs(20)).ok()?;
    (code == 0 && !out.trim().is_empty()).then(|| out.trim().to_string())
}

/// Prints the conduit-guest package version and whether its module is loaded.
pub const VERSION_PROBE: &str = "v=$(dpkg-query -W -f='${Version}' conduit-guest 2>/dev/null || rpm -q --qf '%{VERSION}-%{RELEASE}' conduit-guest 2>/dev/null); [ -n \"$v\" ] || exit 1; if grep -q '^conduit_gpu ' /proc/modules; then echo \"$v (driver loaded)\"; else echo \"$v (driver NOT loaded)\"; fi";

/// Minimal standard base64 (the agent's buf-b64 / out-data).
mod base64_lite {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub fn encode(d: &[u8]) -> String {
        let mut o = String::with_capacity(d.len().div_ceil(3) * 4);
        for c in d.chunks(3) {
            let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
            let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
            for i in 0..4 {
                if i <= c.len() {
                    o.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
                } else {
                    o.push('=');
                }
            }
        }
        o
    }

    pub fn decode(s: &str) -> Result<Vec<u8>, ()> {
        let mut o = Vec::with_capacity(s.len() / 4 * 3);
        let (mut acc, mut bits) = (0u32, 0);
        for ch in s.bytes().filter(|c| !c.is_ascii_whitespace()) {
            if ch == b'=' {
                break;
            }
            let v = A.iter().position(|&a| a == ch).ok_or(())? as u32;
            acc = acc << 6 | v;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                o.push((acc >> bits) as u8);
                acc &= (1 << bits) - 1;
            }
        }
        Ok(o)
    }
}

#[cfg(test)]
mod tests {
    use super::base64_lite::{decode, encode};

    #[test]
    fn base64_roundtrip() {
        for (raw, enc) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(encode(raw.as_bytes()), enc);
            assert_eq!(decode(enc).unwrap(), raw.as_bytes());
        }
        let bin: Vec<u8> = (0..=255u8).collect();
        assert_eq!(decode(&encode(&bin)).unwrap(), bin);
    }

    #[test]
    fn setup_is_idempotent_shell() {
        let s = super::SETUP;
        assert!(s.starts_with("#!/bin/sh"));
        assert!(s.contains("set -eu"));
        assert!(s.contains("systemctl enable conduit-guest.service"));
        for (f, _) in super::FILES {
            assert!(s.contains(f), "setup.sh installs {f}");
        }
    }
}
