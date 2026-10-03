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
Summary:        Conduit guest driver (virtio_gpu_nv), DKMS source
License:        GPL-2.0-only
URL:            https://github.com/olealgoritme/conduit
Source0:        conduit-%{version}.tar.gz
BuildArch:      noarch

Requires:       dkms
Requires:       make gcc
Recommends:     kernel-devel

%description
Kernel module for Linux VMs (kernel 6.4 or newer) running on a Conduit host:
lets NVIDIA's own user-space driver render on the host GPU and provides the
VM's display and input. Built by DKMS for each installed kernel.

%prep
%autosetup -n conduit-%{version}

%build
# Nothing: DKMS builds on the target system.

%install
VERSION=%{version} packaging/build.sh guest-src %{buildroot}%{_usrsrc}/%{name}-%{version}

%post
dkms add -m %{name} -v %{version} -q 2>/dev/null || :
for k in /lib/modules/*/build; do
    kver=$(basename "$(dirname "$k")")
    dkms install -m %{name} -v %{version} -k "$kver" -q 2>/dev/null || :
done

%preun
dkms remove -m %{name} -v %{version} --all -q 2>/dev/null || :

%files
%{_usrsrc}/%{name}-%{version}

%changelog
* Sat Oct 03 2026 Ole Algoritme <olealgoritme@gmail.com> - 0.1.0-1
- Initial package
