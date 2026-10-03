//! One systemd user slice per VM, so its memory is bounded and freed together.
//!
//! Guest RAM is a memfd shared by three processes: the VM runner (QEMU or the
//! built-in one), the GPU backend and virtiofsd. Each is started in its own
//! transient scope inside `conduit-NAME.slice`, and the slice carries the
//! limits: `MemoryMax` = guest RAM + overhead, and `memory.oom.group` so an OOM
//! kill takes the whole set (killing only one of them frees none of the shared
//! RAM). The processes also get a raised `oom_score_adj`, so under global
//! memory pressure the kernel kills a VM before the desktop. `conduit down`
//! stops the slice, which ends every process in it and so releases the memfd.
//!
//! Without a user systemd (or with CONDUIT_NO_SCOPE=1) processes are started
//! plainly, still with the raised `oom_score_adj`.

use crate::mem;
use crate::sys;
use crate::ui;
use std::process::Command;

/// What the kernel's OOM killer adds to a VM process's badness (-1000..1000).
pub const OOM_SCORE_ADJ: &str = "500";

/// The slice of one VM, e.g. `conduit-my\x2dvm.slice` (a `-` would nest it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Slice(String);

pub fn slice_name(vm: &str) -> String {
    let esc: String = vm
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c.to_string()
            } else {
                format!("\\x{:02x}", c as u32)
            }
        })
        .collect();
    format!("conduit-{esc}.slice")
}

/// The MemoryMax of a VM's slice, in MiB.
pub fn memory_max_mib(ram_mib: u64) -> u64 {
    ram_mib + mem::overhead_mib(ram_mib)
}

fn systemctl(args: &[&str]) -> bool {
    sys::quiet("systemctl", &[&["--user"], args].concat())
}

impl Slice {
    /// Set up the VM's slice limits, or None (with a warning) when there is no user systemd.
    pub fn prepare(vm: &str, ram_mib: u64) -> Option<Slice> {
        if std::env::var("CONDUIT_NO_SCOPE").as_deref() == Ok("1") {
            return None;
        }
        let name = slice_name(vm);
        let max = format!("MemoryMax={}M", memory_max_mib(ram_mib));
        if !sys::have("systemd-run") || !systemctl(&["set-property", "--runtime", &name, &max]) {
            ui::warn(
                "no systemd user session: the VM runs without its own memory limit (it may still use all of its RAM)",
            );
            return None;
        }
        Some(Slice(name))
    }

    /// `cmd`, to be run in a new scope in this slice. The scope runs the
    /// program in place (same pid), so pid files keep working.
    pub fn wrap(&self, cmd: &Command) -> Command {
        let mut w = Command::new("systemd-run");
        w.args(["--user", "--scope", "--quiet", "--collect"])
            .arg(format!("--slice={}", self.0))
            .arg("--")
            .arg(cmd.get_program())
            .args(cmd.get_args());
        for (k, v) in cmd.get_envs() {
            match v {
                Some(v) => w.env(k, v),
                None => w.env_remove(k),
            };
        }
        if let Some(d) = cmd.get_current_dir() {
            w.current_dir(d);
        }
        w
    }

    /// Once the slice exists (after its first process started): an OOM kill
    /// inside it takes every process, since one alone frees no guest RAM.
    pub fn group_oom(&self) {
        let Ok(cg) = sys::output(
            "systemctl",
            &["--user", "show", "-p", "ControlGroup", "--value", &self.0],
        ) else {
            return;
        };
        let cg = cg.trim();
        if cg.is_empty() {
            return;
        }
        let _ = std::fs::write(format!("/sys/fs/cgroup{cg}/memory.oom.group"), "1");
    }

    /// Stop every process left in the VM's slice and drop its limits.
    pub fn stop(vm: &str) {
        let name = slice_name(vm);
        if sys::have("systemctl") {
            systemctl(&["stop", &name]);
            systemctl(&["revert", &name]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slice_names_do_not_nest() {
        assert_eq!(slice_name("lab"), "conduit-lab.slice");
        assert_eq!(slice_name("my-vm_2"), "conduit-my\\x2dvm_2.slice");
    }

    #[test]
    fn memory_max_covers_ram_and_overhead() {
        assert_eq!(memory_max_mib(4096), 5120);
        assert_eq!(memory_max_mib(16384), 20480);
    }

    #[test]
    fn wrap_keeps_program_args_and_env() {
        let mut c = Command::new("/bin/backend");
        c.args(["--socket", "/x y"]).env("RUST_LOG", "info");
        let w = Slice("conduit-a.slice".into()).wrap(&c);
        assert_eq!(w.get_program(), "systemd-run");
        let args: Vec<_> = w.get_args().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(
            args,
            [
                "--user",
                "--scope",
                "--quiet",
                "--collect",
                "--slice=conduit-a.slice",
                "--",
                "/bin/backend",
                "--socket",
                "/x y"
            ]
        );
        assert!(w
            .get_envs()
            .any(|(k, v)| k == "RUST_LOG" && v == Some("info".as_ref())));
    }
}
