//! The wizard's tables: package lists, links, the staged first-run checklist
//! and what the log lines mean. Facts live here once; screens and the plain
//! plan both read them.

/// Every link the wizard shows. Nothing else in the module spells a URL.
pub const LINKS: &[(&str, &str)] = &[
    ("omarchy", "https://omarchy.org"),
    (
        "nfpm-releases",
        "https://github.com/goreleaser/nfpm/releases",
    ),
    (
        "nfpm-checksums",
        "https://github.com/goreleaser/nfpm/releases/latest/download/checksums.txt",
    ),
    (
        "nfpm-download",
        "https://github.com/goreleaser/nfpm/releases/download",
    ),
    (
        "conduit-releases",
        "https://github.com/olealgoritme/conduit/releases/latest",
    ),
];

pub fn link(key: &str) -> &'static str {
    LINKS
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, u)| *u)
        .unwrap_or_else(|| panic!("no link {key}"))
}

/// The build and runtime libraries `packaging/build.sh deps` installs, per
/// package manager. A test parses build.sh and fails when these drift.
pub const BUILD_DEPS_APT: &[&str] = &[
    "build-essential",
    "pkg-config",
    "git",
    "curl",
    "ca-certificates",
    "xz-utils",
    "file",
    "patchelf",
    "gnupg",
    "patch",
    "python3",
    "python3-venv",
    "python3-pip",
    "ninja-build",
    "meson",
    "flex",
    "bison",
    "bzip2",
    "musl-tools",
    "libwayland-dev",
    "wayland-protocols",
    "libxcb1-dev",
    "libxcb-dri3-dev",
    "libxcb-present-dev",
    "libxcb-render0-dev",
    "libxcb-xinput-dev",
    "libgbm-dev",
    "libssl-dev",
    "libegl-dev",
    "libglib2.0-dev",
    "libpixman-1-dev",
    "libslirp-dev",
    "libseccomp-dev",
    "libcap-ng-dev",
    "libzstd-dev",
    "libaio-dev",
    "libfdt-dev",
    "libpulse-dev",
    "libpipewire-0.3-dev",
    "libvulkan-dev",
    "libdrm-dev",
    "python3-mako",
    "python3-yaml",
];

pub const BUILD_DEPS_DNF: &[&str] = &[
    "gcc",
    "gcc-c++",
    "make",
    "pkgconf-pkg-config",
    "git",
    "curl",
    "xz",
    "file",
    "patchelf",
    "gnupg2",
    "patch",
    "python3",
    "ninja-build",
    "meson",
    "flex",
    "bison",
    "bzip2",
    "diffutils",
    "findutils",
    "rpm-build",
    "wayland-devel",
    "wayland-protocols-devel",
    "libxcb-devel",
    "mesa-libgbm-devel",
    "openssl-devel",
    "mesa-libEGL-devel",
    "glib2-devel",
    "pixman-devel",
    "libslirp-devel",
    "libseccomp-devel",
    "libcap-ng-devel",
    "libzstd-devel",
    "libaio-devel",
    "libfdt-devel",
    "pulseaudio-libs-devel",
    "pipewire-devel",
    "vulkan-headers",
    "vulkan-loader-devel",
    "libdrm-devel",
    "python3-mako",
    "python3-pyyaml",
];

pub const BUILD_DEPS_PACMAN: &[&str] = &[
    "base-devel",
    "git",
    "curl",
    "xz",
    "file",
    "patchelf",
    "gnupg",
    "python",
    "ninja",
    "meson",
    "flex",
    "bison",
    "wayland",
    "wayland-protocols",
    "libxcb",
    "mesa",
    "openssl",
    "glib2",
    "pixman",
    "libslirp",
    "libseccomp",
    "libcap-ng",
    "zstd",
    "libaio",
    "dtc",
    "libpulse",
    "pipewire",
    "vulkan-headers",
    "vulkan-icd-loader",
    "libdrm",
    "python-mako",
    "python-yaml",
];

/// libvirt, virt-manager and what a domain built by the wizard needs
/// (an emulator, qemu-img for the disk, OVMF for UEFI). Not in build.sh.
pub const LIBVIRT_APT: &[&str] = &[
    "libvirt-daemon",
    "libvirt-daemon-system",
    "libvirt-clients",
    "virtinst",
    "virt-manager",
    "qemu-system-x86",
    "qemu-utils",
    "ovmf",
];
pub const LIBVIRT_DNF: &[&str] = &[
    "libvirt-daemon-kvm",
    "libvirt-client",
    "virt-install",
    "virt-manager",
    "qemu-img",
    "edk2-ovmf",
];
pub const LIBVIRT_PACMAN: &[&str] = &[
    "libvirt",
    "qemu-base",
    "virt-install",
    "virt-manager",
    "edk2-ovmf",
];

/// One stage of the first run: do this, expect that, stop if not.
pub struct Stage {
    pub title: &'static str,
    pub command: &'static str,
    pub expect: &'static str,
}

/// Go up one stage at a time; stop at the first one that misbehaves.
pub const STAGES: &[Stage] = &[
    Stage {
        title: "1. Module load only",
        command: "conduit view NAME   (log in, then leave the desktop idle for a minute)",
        expect: "The VM boots, the guest driver loads (`lsmod | grep conduit` in the VM), the host log shows no NVRM or Xid lines.",
    },
    Stage {
        title: "2. vulkaninfo",
        command: "vulkaninfo --summary   (in the VM)",
        expect: "Your GPU is listed through NVK or the NVIDIA driver, with no error lines.",
    },
    Stage {
        title: "3. vkcube",
        command: "vkcube   (in the VM)",
        expect: "A spinning cube at your display's refresh rate. Close it with Ctrl+C before trying a game.",
    },
];

/// Log lines worth knowing. `needle` is matched against a line.
pub struct LogLine {
    pub needle: &'static str,
    pub meaning: &'static str,
}

pub const LOG_LINES: &[LogLine] = &[
    LogLine {
        needle: "page kind 6/2",
        meaning: "Backend log, `conduit logs NAME backend`: the tiling layout the host card reports and guests copy (docs/GPU-SUPPORT.md). Seen once per GPU at start; expected.",
    },
    LogLine {
        needle: "safe mode",
        meaning: "Backend log: safe mode is on (2 GiB video-memory cap, 1 s limits on blocking GPU calls). Expected for a closed or older driver.",
    },
    LogLine {
        needle: "NVRM",
        meaning: "Host kernel log (`journalctl -k -f`): the NVIDIA driver speaking. Anything about a failed or rejected request: stop and report it.",
    },
    LogLine {
        needle: "Xid",
        meaning: "Host kernel log: the GPU reported a fault. Any Xid line while a VM runs: stop the VM (`conduit poweroff NAME`) and report it.",
    },
];

/// The command to watch the host kernel log during a first run.
pub const WATCH_KERNEL_LOG: &str = "journalctl -k -f | grep -E 'NVRM|Xid'";
