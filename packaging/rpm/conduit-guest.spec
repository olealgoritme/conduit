# RPM spec for the Conduit guest driver as a DKMS source package (noarch).
# Install inside the VM. Release .rpm files are made by nfpm with the same
# layout and scripts; this spec is for COPR/OBS/local builds.
#
# DKMS rather than akmod: one noarch package works on Fedora, RHEL (EPEL dkms)
# and openSUSE alike. An akmod variant (kmodtool) is described in
# docs/PACKAGING.md if RPM Fusion-style packaging is wanted later.

Name:           conduit-guest
Version:        0.1.0
Release:        1%{?dist}
Summary:        Conduit guest driver (conduit_gpu), DKMS source
License:        GPL-2.0-only
URL:            https://github.com/olealgoritme/conduit
Source0:        conduit-%{version}.tar.gz
BuildArch:      noarch

Requires:       dkms
Requires:       make gcc
Requires:       python3
Recommends:     kernel-devel
Recommends:     wl-clipboard libX11 libXfixes

%description
Kernel module for Linux VMs (kernel 6.4 or newer) running on a Conduit host:
lets NVIDIA's own user-space driver render on the host GPU and provides the
VM's display, input and clipboard. Built by DKMS for each installed kernel.
Includes conduit-clipboard-agent, which shares each desktop session's
clipboard with the host.

%prep
%autosetup -n conduit-%{version}

%build
# Nothing: DKMS builds on the target system.

%install
VERSION=%{version} packaging/build.sh guest-src %{buildroot}%{_usrsrc}/%{name}-%{version}
install -D -m0755 guest/agent/conduit-clipboard-agent %{buildroot}%{_bindir}/conduit-clipboard-agent
install -D -m0644 guest/agent/conduit-clipboard.service %{buildroot}%{_userunitdir}/conduit-clipboard.service
install -D -m0644 guest/agent/conduit-clipboard.desktop %{buildroot}%{_sysconfdir}/xdg/autostart/conduit-clipboard.desktop
install -D -m0644 guest/agent/70-conduit-clipboard.rules %{buildroot}%{_udevrulesdir}/70-conduit-clipboard.rules
install -D -m0644 guest/power/50-conduit-powerkey.conf %{buildroot}/usr/lib/systemd/logind.conf.d/50-conduit-powerkey.conf
install -D -m0644 guest/agent/README.md %{buildroot}%{_docdir}/%{name}/README.clipboard.md
install -D -m0644 guest/system/modules-load.conf %{buildroot}%{_modulesloaddir}/conduit-gpu.conf
install -D -m0644 guest/system/modprobe.conf %{buildroot}%{_modprobedir}/conduit-gpu.conf
install -D -m0644 guest/system/60-conduit-userns.conf %{buildroot}%{_sysconfdir}/sysctl.d/60-conduit-userns.conf
install -D -m0755 guest/system/conduit-guest-setup %{buildroot}%{_prefix}/lib/conduit-guest/setup

%post
# Retires the old module name (virtio_gpu_nv), sets a locale, applies sysctl.
%{_prefix}/lib/conduit-guest/setup || :
udevadm control --reload-rules 2>/dev/null || :
udevadm trigger --subsystem-match=misc --sysname-match=conduit-clipboard 2>/dev/null || :
systemctl --global enable conduit-clipboard.service 2>/dev/null || :
dkms add -m %{name} -v %{version} -q 2>/dev/null || :
for k in /lib/modules/*/build; do
    kver=$(basename "$(dirname "$k")")
    dkms install -m %{name} -v %{version} -k "$kver" -q 2>/dev/null || :
done
echo "conduit-guest: reboot the VM to load the guest driver (conduit_gpu)."

%preun
if [ "$1" = 0 ]; then
    systemctl --global disable conduit-clipboard.service 2>/dev/null || :
fi
dkms remove -m %{name} -v %{version} --all -q 2>/dev/null || :

%files
%{_usrsrc}/%{name}-%{version}
%{_bindir}/conduit-clipboard-agent
%{_userunitdir}/conduit-clipboard.service
%config(noreplace) %{_sysconfdir}/xdg/autostart/conduit-clipboard.desktop
%{_udevrulesdir}/70-conduit-clipboard.rules
/usr/lib/systemd/logind.conf.d/50-conduit-powerkey.conf
%{_modulesloaddir}/conduit-gpu.conf
%{_modprobedir}/conduit-gpu.conf
%config(noreplace) %{_sysconfdir}/sysctl.d/60-conduit-userns.conf
%{_prefix}/lib/conduit-guest
%doc %{_docdir}/%{name}/README.clipboard.md

%changelog
* Sat Oct 03 2026 Ole Algoritme <olealgoritme@gmail.com> - 0.1.0-1
- Initial package
