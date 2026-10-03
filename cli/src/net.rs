//! The VM's private network: a tap device owned by you, plus NAT so the VM
//! reaches the internet through whatever uplink the host has (wifi, ethernet,
//! a VPN that comes and goes: the masquerade rule names no interface).
//! Idempotent both ways.

use crate::sys::{self, quiet, sudo, sudo_ok};
use crate::vm::VmConfig;
use anyhow::{Context, Result};

fn tap_exists(tap: &str) -> bool {
    quiet("ip", &["link", "show", tap])
}

fn tap_ready(c: &VmConfig) -> bool {
    let n = c.net();
    let Ok(out) = sys::output("ip", &["-4", "-br", "addr", "show", "dev", &n.tap]) else { return false };
    out.contains(&format!("{}/", n.host_ip)) && (out.contains(" UP ") || out.contains("UNKNOWN"))
}

fn rules(c: &VmConfig) -> Vec<Vec<String>> {
    let n = c.net();
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    vec![
        s(&["-t", "nat", "POSTROUTING", "-s", &n.subnet, "!", "-d", &n.subnet, "-j", "MASQUERADE"]),
        s(&["FORWARD", "-i", &n.tap, "-j", "ACCEPT"]),
        s(&["FORWARD", "-o", &n.tap, "-m", "state", "--state", "RELATED,ESTABLISHED", "-j", "ACCEPT"]),
    ]
}

/// Split ["-t","nat","CHAIN",rest..] into the table part and the rule.
fn with_op(rule: &[String], op: &str) -> Vec<String> {
    let mut v = Vec::new();
    let mut r = rule;
    if r.first().map(String::as_str) == Some("-t") {
        v.extend_from_slice(&r[..2]);
        r = &r[2..];
    }
    v.push(op.to_string());
    v.extend_from_slice(r);
    v
}

fn ipt(args: &[String]) -> Vec<&str> {
    args.iter().map(String::as_str).collect()
}

fn rules_present(c: &VmConfig) -> bool {
    rules(c).iter().all(|r| sudo_ok("iptables", &ipt(&with_op(r, "-C"))))
}

pub fn up(c: &VmConfig) -> Result<()> {
    let n = c.net();
    // Fast path: the tap is already set up and (if we can check without a
    // password) so are the rules. Avoids asking for sudo on every start.
    if tap_ready(c) && (!sys::quiet("sudo", &["-n", "true"]) || rules_present(c)) {
        return Ok(());
    }
    sys::sudo_ready(&format!("Setting up the VM's private network ({} on {}).", n.tap, n.subnet))?;
    let user = crate::paths::username();
    if !tap_exists(&n.tap) {
        sudo("ip", &["tuntap", "add", "dev", &n.tap, "mode", "tap", "user", &user])
            .context("could not create the VM's network device")?;
    }
    let addrs = sys::output("ip", &["-4", "addr", "show", "dev", &n.tap]).unwrap_or_default();
    if !addrs.contains(&format!("{}/", n.host_ip)) {
        sudo("ip", &["addr", "add", &format!("{}/24", n.host_ip), "dev", &n.tap])?;
    }
    sudo("ip", &["link", "set", &n.tap, "up"])?;
    if std::fs::read_to_string("/proc/sys/net/ipv4/ip_forward").unwrap_or_default().trim() != "1" {
        sudo("sysctl", &["-qw", "net.ipv4.ip_forward=1"])?;
    }
    for (i, r) in rules(c).iter().enumerate() {
        if !sudo_ok("iptables", &ipt(&with_op(r, "-C"))) {
            // NAT appends; FORWARD inserts at the top so a DROP policy does not shadow us.
            let op = if i == 0 { "-A" } else { "-I" };
            sudo("iptables", &ipt(&with_op(r, op))).context("could not set up the VM's internet access")?;
        }
    }
    Ok(())
}

/// Remove rules and tap. ip_forward is left alone: Docker/libvirt rely on it.
/// With `interactive` false (background), it only proceeds if sudo needs no password.
pub fn down(c: &VmConfig, interactive: bool) -> Result<()> {
    let n = c.net();
    // Rules and tap are added and removed together; no tap means nothing to do
    // (and no password prompt for it).
    if !tap_exists(&n.tap) {
        return Ok(());
    }
    if !interactive && !sys::quiet("sudo", &["-n", "true"]) && unsafe { libc::geteuid() } != 0 {
        anyhow::bail!("network {} left in place (needs sudo); `conduit down {}` removes it", n.tap, c.name);
    }
    sys::sudo_ready(&format!("Removing the VM's private network ({}).", n.tap))?;
    for r in rules(c) {
        let mut guard = 0;
        while sudo_ok("iptables", &ipt(&with_op(&r, "-D"))) && guard < 32 {
            guard += 1;
        }
    }
    if tap_exists(&n.tap) {
        sudo("ip", &["link", "del", &n.tap])?;
    }
    Ok(())
}

pub fn describe(c: &VmConfig) -> String {
    let n = c.net();
    if tap_ready(c) {
        format!("up ({} {} -> VM {})", n.tap, n.host_ip, n.guest_ip)
    } else if tap_exists(&n.tap) {
        format!("{} present but not configured", n.tap)
    } else {
        "down".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_ops() {
        let c = VmConfig::new("t", 1024, 1, 2, "u", "none");
        let r = rules(&c);
        assert_eq!(
            with_op(&r[0], "-C").join(" "),
            "-t nat -C POSTROUTING -s 172.30.2.0/24 ! -d 172.30.2.0/24 -j MASQUERADE"
        );
        assert_eq!(with_op(&r[1], "-I").join(" "), "-I FORWARD -i conduit2 -j ACCEPT");
        // No uplink interface is named anywhere: VPNs come and go.
        assert!(r.iter().all(|x| !x.contains(&"-o".to_string()) || x.contains(&"conduit2".to_string())));
    }
}
