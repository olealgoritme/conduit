//! Power control from the CLI, the same as virt-manager's buttons, for libvirt
//! VMs and for VMs `conduit up` runs directly: shutdown, reboot, reset,
//! poweroff, pause, resume.

use crate::qemu;
use crate::run::{self, Rt, Stop};
use crate::sys;
use crate::ui::{self, oops};
use crate::virt::{self, Link};
use crate::vm::VmConfig;
use anyhow::Result;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Power {
    /// Power button; wait up to the timeout; never forced.
    Shutdown(Duration),
    /// Ask the guest to restart.
    Reboot,
    /// Hard reset (the reset button).
    Reset,
    /// Pull the plug.
    Poweroff,
    Pause,
    Resume,
}

fn not_running(name: &str) -> anyhow::Error {
    oops(
        format!("{name} is not running"),
        format!("Start it with `conduit up {name}` or `conduit view {name}`"),
    )
}

pub fn power(name: &str, p: Power) -> Result<()> {
    if let Some(link) = Link::load(name) {
        return libvirt(name, &link, p);
    }
    direct(name, p)
}

fn libvirt(name: &str, link: &Link, p: Power) -> Result<()> {
    let v = link.virsh();
    v.reachable()?;
    let d = link.domain.as_str();
    let state = v.state(d).unwrap_or_default();
    if !virt::state_is_up(&state) {
        return Err(not_running(name));
    }
    match p {
        Power::Shutdown(t) => {
            if state == "paused" {
                v.run(&["resume", d])?;
            }
            v.run(&["shutdown", d])?;
            ui::info(format!(
                "asked {name} to shut down; waiting up to {} s…",
                t.as_secs()
            ));
            if !sys::wait_for(t, || !v.state(d).is_some_and(|s| virt::state_is_up(&s))) {
                return Err(oops(
                    format!(
                        "{name} is still running after {} s (it ignored the power button)",
                        t.as_secs()
                    ),
                    format!(
                        "`conduit poweroff {name}` forces it off; `conduit down {name}` does both"
                    ),
                ));
            }
            crate::lvrun::after_stop(name, link);
            ui::info(format!("{name} is stopped"));
        }
        Power::Reboot => {
            v.run(&["reboot", d])?;
            ui::info(format!("{name} is rebooting"));
        }
        Power::Reset => {
            v.run(&["reset", d])?;
            ui::info(format!("{name} was reset"));
        }
        Power::Poweroff => return run::down(name, Stop::Force),
        // Idempotent, like QMP's stop/cont: pausing a paused VM or resuming
        // a running one succeeds, so the viewer's Pause / Resume toggle (which
        // only knows what its own commands did) puts itself right.
        Power::Pause if state == "paused" => {
            ui::info(format!(
                "{name} is already paused (`conduit resume {name}`)"
            ));
        }
        Power::Pause => {
            keep_gpu_state(name);
            v.run(&["suspend", d])?;
            ui::info(format!("{name} is paused (`conduit resume {name}`)"));
        }
        Power::Resume if !state.is_empty() && state != "paused" => {
            ui::info(format!("{name} is already running"));
        }
        Power::Resume => {
            v.run(&["resume", d])?;
            gpu_state_kept(name);
            ui::info(format!("{name} is running"));
        }
    }
    Ok(())
}

/// QEMU stops the GPU device while a VM is paused and starts it again when
/// it continues; the backend takes such a restart for a guest reboot unless
/// told to keep the guest's GPU objects (`keep` on its power socket,
/// docs/SUSPEND.md). A backend without the socket (started by an older
/// conduit) releases them as before.
fn keep_gpu_state(name: &str) {
    if let Err(e) = crate::sleep::backend_ask(name, "keep", Duration::from_secs(1)) {
        ui::warn(format!(
            "{name}'s GPU backend cannot keep its GPU state across the pause ({e:#}): \
             programs using the GPU in the VM may fail after `conduit resume`; restart the \
             VM once to get a backend that can"
        ));
    }
}

/// After the continue: the restart it caused has been kept (or comes soon,
/// within the backend's grace period); a later one is a reboot again.
fn gpu_state_kept(name: &str) {
    let _ = crate::sleep::backend_ask(name, "release", Duration::from_secs(10));
}

/// A VM `conduit up`/`view` runs directly: QMP for QEMU, ssh for the guest.
fn direct(name: &str, p: Power) -> Result<()> {
    let c = VmConfig::load(name)?;
    if !run::is_running_unmanaged(name) {
        return Err(not_running(name));
    }
    let rt = Rt::new(name)?;
    let st = rt.state();
    let qemu_vm = st.vmm == "qemu";
    let qmp = rt.p("qmp.sock");
    let need_qemu = |what: &str| -> Result<()> {
        if qemu_vm {
            Ok(())
        } else {
            Err(oops(
                format!("{what} needs the QEMU runner; {name} runs under the built-in one"),
                format!("Restart it under QEMU: conduit down {name}; conduit up {name} --vmm qemu"),
            ))
        }
    };
    let qmp_do = |cmd: &str, done: &str| -> Result<()> {
        if qemu::ask(&qmp, cmd) {
            ui::info(format!("{name} {done}"));
            Ok(())
        } else {
            Err(oops(
                format!("QEMU did not accept {cmd}"),
                format!("See `conduit status {name}`"),
            ))
        }
    };
    match p {
        Power::Shutdown(t) => {
            let pid = rt.pid("vm", &st.vm_comm).ok_or_else(|| not_running(name))?;
            let asked =
                (qemu_vm && qemu::ask(&qmp, "system_powerdown")) | run::guest_ask(&c, "poweroff");
            if !asked {
                return Err(oops(
                    format!("could not reach {name} to shut it down"),
                    format!("`conduit poweroff {name}` forces it off"),
                ));
            }
            ui::info(format!(
                "asked {name} to shut down; waiting up to {} s…",
                t.as_secs()
            ));
            if !sys::wait_for(t, || !sys::alive(pid)) {
                return Err(oops(
                    format!("{name} is still running after {} s", t.as_secs()),
                    format!(
                        "`conduit poweroff {name}` forces it off; `conduit down {name}` does both"
                    ),
                ));
            }
            // Gone: clean up the rest (backend, network, window).
            run::down(name, Stop::Force)
        }
        Power::Reboot => {
            if run::guest_ask(&c, "reboot") {
                ui::info(format!("{name} is rebooting"));
                Ok(())
            } else {
                Err(oops(
                    format!("could not reach {name} over ssh to reboot it"),
                    format!("`conduit reset {name}` resets it the hard way"),
                ))
            }
        }
        Power::Reset => {
            need_qemu("reset")?;
            qmp_do("system_reset", "was reset")
        }
        Power::Poweroff => run::down(name, Stop::Force),
        Power::Pause => {
            need_qemu("pause")?;
            keep_gpu_state(name);
            qmp_do("stop", &format!("is paused (`conduit resume {name}`)"))
        }
        Power::Resume => {
            need_qemu("resume")?;
            let r = qmp_do("cont", "is running");
            gpu_state_kept(name);
            r
        }
    }
}
