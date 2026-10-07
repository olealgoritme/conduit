//! One recipe per kind of guest: its steps as data, derived from what the
//! wizard knows (`Env`). A step's check looks at the real world, so a step
//! the user already did by hand shows as done.

use super::data::link;
use super::domain::{CD_TARGET, SESSION_URI};
use super::hostfix::libvirt_fix;
use super::{CheckResult, Env, Field, Fix, Step, Verify};

pub struct Recipe {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub recommended: bool,
    /// Not automated: the page that explains it.
    pub experimental_doc: Option<&'static str>,
    pub default_name: &'static str,
    pub steps: fn(&Env) -> Vec<Step>,
}

pub fn all() -> Vec<Recipe> {
    vec![
        Recipe {
            id: "ubuntu",
            name: "Ubuntu 24.04 (automatic)",
            description: "Conduit downloads Ubuntu, installs a desktop and the GPU driver by itself: `conduit create`. Nothing to click in an installer.",
            recommended: true,
            experimental_doc: None,
            default_name: "ubuntu",
            steps: ubuntu_steps,
        },
        Recipe {
            id: "omarchy",
            name: "Omarchy (guided install)",
            description: "You install Omarchy from its ISO in a console window; Conduit defines the VM for it, then adds the GPU and the guest driver.",
            recommended: true,
            experimental_doc: None,
            default_name: "omarchy",
            steps: omarchy_steps,
        },
        Recipe {
            id: "existing",
            name: "A VM I already have in libvirt",
            description: "Give a VM from virt-manager or virsh the GPU: `conduit attach`. Its definition is backed up first.",
            recommended: false,
            experimental_doc: None,
            default_name: "",
            steps: existing_steps,
        },
        Recipe {
            id: "win11",
            name: "Windows 11 (experimental)",
            description: "Not set up by this wizard. docs/WINDOWS.md has the steps and the known limits.",
            recommended: false,
            experimental_doc: Some("docs/WINDOWS.md"),
            default_name: "",
            steps: |_| Vec::new(),
        },
    ]
}

fn virsh(args: &[&str]) -> Vec<String> {
    ["virsh", "-c", SESSION_URI]
        .iter()
        .chain(args.iter())
        .map(|s| s.to_string())
        .collect()
}

fn name_step() -> Step {
    Step::new(
        "vm-name",
        "Name the VM",
        "Letters, digits, - and _. It is the name you type in every conduit command.",
        |env: &Env| match crate::vm::check_name(&env.vm_name) {
            Ok(()) => CheckResult::Done(env.vm_name.clone()),
            Err(_) => CheckResult::Todo("pick a name".into()),
        },
        Some(Fix::Ask {
            field: Field::VmName,
        }),
        Verify::Recheck,
    )
}

fn libvirt_step(env: &Env) -> Step {
    Step::new(
        "libvirt",
        "libvirt and virt-manager",
        "libvirt runs the VM and virt-manager gives you a console to install the guest in. This is the user session (qemu:///session): no root daemon.",
        |env: &Env| {
            if env.libvirt_session {
                CheckResult::Done("qemu:///session answers".into())
            } else if env.virsh {
                CheckResult::Todo("installed, but qemu:///session does not answer (`virsh -c qemu:///session list`)".into())
            } else {
                CheckResult::Todo("not installed".into())
            }
        },
        Some(libvirt_fix(env)),
        Verify::Recheck,
    )
}

fn ubuntu_steps(env: &Env) -> Vec<Step> {
    let n = env.vm_name.clone();
    vec![
        name_step(),
        Step::new(
            "create",
            "Make the VM",
            "Downloads Ubuntu 24.04 (checked against Ubuntu's signed checksums), builds the disk with the desktop and the GPU driver, and registers it with libvirt. Takes several minutes and asks for sudo to build the disk.",
            |env: &Env| {
                if env.conduit_vms.contains(&env.vm_name) {
                    CheckResult::Done("the VM exists".into())
                } else {
                    CheckResult::Todo("not made yet".into())
                }
            },
            Some(Fix::Run {
                cmd: vec!["conduit".into(), "create".into(), n.clone()],
                needs_sudo: true,
            }),
            Verify::Recheck,
        ),
        Step::new(
            "doctor-vm",
            "Check the VM's chain",
            "Looks at the disk, sockets, backend and driver of this one VM.",
            |env: &Env| {
                if env.is_confirmed("doctor-vm") {
                    CheckResult::Done("checked".into())
                } else {
                    CheckResult::Warn("not run".into())
                }
            },
            Some(Fix::Run {
                cmd: vec!["conduit".into(), "doctor".into(), n.clone()],
                needs_sudo: false,
            }),
            Verify::Exit0,
        ),
    ]
}

fn omarchy_steps(env: &Env) -> Vec<Step> {
    let n = env.vm_name.clone();
    let iso = env.iso.clone();
    vec![
        name_step(),
        libvirt_step(env),
        Step::new(
            "iso-page",
            "Get the Omarchy ISO",
            &format!("Download the installer image from the project's own page, {}. Conduit does not link a file directly, because those links go stale.", link("omarchy")),
            |env: &Env| {
                if env.iso.is_some() {
                    CheckResult::Done("you have an image".into())
                } else {
                    CheckResult::Warn("not downloaded yet".into())
                }
            },
            Some(Fix::Open { url: link("omarchy").into() }),
            Verify::Recheck,
        ),
        Step::new(
            "iso",
            "Say where the ISO is",
            "The path of the downloaded .iso file.",
            |env: &Env| match &env.iso {
                Some(p) if std::path::Path::new(p).is_file() => CheckResult::Done(p.clone()),
                Some(p) => CheckResult::Todo(format!("{p} is not a file")),
                None => CheckResult::Todo("no path given".into()),
            },
            Some(Fix::Ask { field: Field::IsoPath }),
            Verify::Recheck,
        ),
        iso_checksum_step(env),
        Step::new(
            "define",
            "Define the VM in libvirt",
            "Creates a sparse 64G qcow2 disk and a UEFI VM (secure boot off, virtio disk, user-mode network, VNC on 127.0.0.1, a guest agent channel) with the ISO in a CD drive. It does not start it.",
            |env: &Env| {
                if env.domain.defined {
                    CheckResult::Done("defined".into())
                } else {
                    CheckResult::Todo("not defined".into())
                }
            },
            iso.as_ref().map(|p| Fix::Run {
                cmd: vec!["conduit".into(), "setup".into(), "define".into(), n.clone(), "--iso".into(), p.clone()],
                needs_sudo: false,
            }),
            Verify::Recheck,
        ),
        Step::new(
            "start",
            "Start the VM for the installer",
            "Boots the ISO. Conduit's GPU is not attached yet, so this is a plain VM.",
            |env: &Env| match env.domain.state.as_deref() {
                Some(s) if crate::virt::state_is_up(s) => CheckResult::Done("running".into()),
                _ => CheckResult::Todo("not running".into()),
            },
            Some(Fix::Run { cmd: virsh(&["start", &n]), needs_sudo: false }),
            Verify::Recheck,
        ),
        Step::new(
            "install-os",
            "Install Omarchy in the console",
            "Installs Omarchy onto the VM's virtual disk from the ISO, in a console window.",
            |env: &Env| {
                if env.is_confirmed("install-os") {
                    CheckResult::Done("installed".into())
                } else {
                    CheckResult::Todo("do this in the console".into())
                }
            },
            Some(Fix::guide(format!("Open the console:\n  virt-viewer -c {SESSION_URI} {n}\n\nInstall Omarchy to the virtual disk, restart when it asks, and log in once. Press Enter here when it is installed."))),
            Verify::Confirm,
        ),
        Step::new(
            "eject",
            "Remove the installer CD",
            "Ejects the ISO from the CD drive so the VM boots from its disk.",
            |env: &Env| {
                if env.domain.cd_inserted {
                    CheckResult::Todo("the ISO is still in the drive".into())
                } else {
                    CheckResult::Done("no CD".into())
                }
            },
            Some(Fix::Run { cmd: virsh(&["change-media", &n, CD_TARGET, "--eject", "--config"]), needs_sudo: false }),
            Verify::Recheck,
        ),
        agent_step(),
        attach_step(&n),
        shutdown_step(&n),
    ]
}

fn agent_step() -> Step {
    Step::new(
        "guest-agent",
        "QEMU guest agent in the VM",
        "Conduit installs the guest driver through the guest agent, so the agent must run inside the VM (the VM must be running).",
        |env: &Env| {
            if env.domain.agent {
                CheckResult::Done("the agent answers".into())
            } else {
                CheckResult::Todo("no answer from the agent".into())
            }
        },
        Some(Fix::guide(
            "Inside the VM, in a terminal:\n  Arch / Omarchy:  sudo pacman -S --needed qemu-guest-agent && sudo systemctl enable --now qemu-guest-agent\n  Ubuntu / Debian: sudo apt install qemu-guest-agent && sudo systemctl enable --now qemu-guest-agent\n  Fedora:          sudo dnf install qemu-guest-agent && sudo systemctl enable --now qemu-guest-agent\nThen press Enter and r to check again.",
        )),
        Verify::Recheck,
    )
}

fn attach_step(n: &str) -> Step {
    Step::new(
        "attach",
        "Give the VM Conduit's GPU",
        "Backs up the VM's definition, adds Conduit's GPU device and units, and installs the guest driver through the agent. `conduit detach` undoes it.",
        |env: &Env| {
            if env.domain.attached {
                CheckResult::Done("attached".into())
            } else {
                CheckResult::Todo("not attached".into())
            }
        },
        Some(Fix::Run {
            cmd: vec!["conduit".into(), "attach".into(), n.into()],
            needs_sudo: true,
        }),
        Verify::Recheck,
    )
}

fn shutdown_step(n: &str) -> Step {
    Step::new(
        "shutdown",
        "Shut the VM down fully",
        "The GPU device only takes effect on a fresh start, so power the VM off now (not suspend, not restart). `conduit view` starts it again.",
        |env: &Env| match env.domain.state.as_deref() {
            Some("shut off") => CheckResult::Done("shut off".into()),
            Some(s) => CheckResult::Todo(s.into()),
            None => CheckResult::Todo("unknown".into()),
        },
        Some(Fix::Run {
            cmd: vec!["conduit".into(), "shutdown".into(), n.into()],
            needs_sudo: false,
        }),
        Verify::Recheck,
    )
}

fn iso_checksum_step(env: &Env) -> Step {
    let fix = match (&env.iso, &env.iso_sha256) {
        (Some(p), Some(h)) => Some(Fix::Run {
            cmd: vec![
                "sh".into(),
                "-c".into(),
                format!("echo '{h}  {}' | sha256sum -c -", p.replace('\'', "'\\''")),
            ],
            needs_sudo: false,
        }),
        _ => Some(Fix::Ask {
            field: Field::IsoSha256,
        }),
    };
    Step::new(
        "iso-sha256",
        "Check the ISO's checksum (optional)",
        "If the download page gives a sha256, paste it and Conduit checks the file. Leave it empty to skip.",
        |env: &Env| match &env.iso_sha256 {
            None => CheckResult::Done("skipped".into()),
            Some(_) if env.is_confirmed("iso-sha256") => CheckResult::Done("matches".into()),
            Some(_) => CheckResult::Todo("not checked yet".into()),
        },
        fix,
        Verify::Exit0,
    )
}

fn existing_steps(env: &Env) -> Vec<Step> {
    let n = env.vm_name.clone();
    vec![
        name_step(),
        libvirt_step(env),
        Step::new(
            "defined",
            "The VM exists in libvirt",
            "Conduit looks for the name in the user session. A VM in qemu:///system: run `conduit attach NAME -c qemu:///system` yourself.",
            |env: &Env| {
                if env.domain.defined {
                    CheckResult::Done("found".into())
                } else {
                    CheckResult::Todo("no VM with this name".into())
                }
            },
            Some(Fix::Ask { field: Field::VmName }),
            Verify::Recheck,
        ),
        agent_step(),
        attach_step(&n),
        shutdown_step(&n),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn four_recipes_two_recommended_and_windows_is_not_automated() {
        let r = all();
        let ids: Vec<_> = r.iter().map(|r| r.id).collect();
        assert_eq!(ids, ["ubuntu", "omarchy", "existing", "win11"]);
        assert_eq!(r.iter().filter(|r| r.recommended).count(), 2);
        let w = r.iter().find(|r| r.id == "win11").unwrap();
        assert_eq!(w.experimental_doc, Some("docs/WINDOWS.md"));
        assert!((w.steps)(&Env::fixture()).is_empty());
    }

    #[test]
    fn omarchy_hardcodes_no_download_url_only_the_project_page() {
        let env = Env::fixture();
        for s in omarchy_steps(&env) {
            let text = format!("{} {:?}", s.explanation, s.fix);
            for w in text.split_whitespace().filter(|w| w.contains("http")) {
                assert!(
                    w.trim_matches(|c: char| ",.)\"".contains(c))
                        .starts_with(link("omarchy")),
                    "{w}"
                );
            }
        }
    }
}
