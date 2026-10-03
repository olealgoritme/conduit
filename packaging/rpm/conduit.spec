# RPM spec for building Conduit from source (COPR, OBS, local rpmbuild).
# Release .rpm files are made by nfpm in .github/workflows/release.yml; both
# paths run packaging/build.sh, so the installed layout is identical.
#
#   git archive --prefix=conduit-%{version}/ -o ~/rpmbuild/SOURCES/conduit-%{version}.tar.gz v%{version}
#   rpmbuild -ba packaging/rpm/conduit.spec
#
# Network access is needed during %build (cargo crates, QEMU tarball) unless
# the sources are vendored first; COPR: enable "network in build".

%global debug_package %{nil}
# Bundled QEMU lives in /opt/conduit; do not let rpm "provide" its libraries.
%global __provides_exclude_from ^/opt/conduit/.*$

Name:           conduit
Version:        0.1.0
Release:        1%{?dist}
Summary:        Share your NVIDIA GPU with a virtual machine
License:        Apache-2.0 AND BSD-3-Clause AND GPL-2.0-only
URL:            https://github.com/olealgoritme/conduit
Source0:        %{name}-%{version}.tar.gz
ExclusiveArch:  x86_64

BuildRequires:  cargo >= 1.90
BuildRequires:  rust >= 1.90
BuildRequires:  gcc gcc-c++ make pkgconf-pkg-config file
BuildRequires:  python3 ninja-build meson flex bison bzip2 diffutils findutils
BuildRequires:  wayland-devel wayland-protocols-devel libxcb-devel mesa-libgbm-devel
BuildRequires:  openssl-devel mesa-libEGL-devel
BuildRequires:  glib2-devel pixman-devel libslirp-devel libseccomp-devel
BuildRequires:  pulseaudio-libs-devel pipewire-devel
BuildRequires:  libcap-ng-devel libzstd-devel libaio-devel libfdt-devel
Recommends:     policycoreutils-python-utils
Suggests:       libvirt-daemon virt-manager

%description
Conduit lets a Linux VM use your real NVIDIA graphics card while your own
desktop keeps using it too. This package has the host side: the GPU backend,
the zero-copy viewer, a built-in VM runner and a bundled QEMU 11.1 in
/opt/conduit, plus the `conduit` command.

%prep
%autosetup -n %{name}-%{version}

%build
export VERSION=%{version} RUST_TARGET=host JOBS=%{_smp_build_ncpus}
packaging/build.sh rust
packaging/build.sh viewer
packaging/build.sh stream
packaging/build.sh qemu

%install
VERSION=%{version} RUST_TARGET=host STAGE=%{buildroot} packaging/build.sh stage

%post
/opt/conduit/libexec/conduit-integrate enable || :

%preun
if [ $1 -eq 0 ]; then
    /opt/conduit/libexec/conduit-integrate disable || :
fi

%files
/opt/conduit
%{_bindir}/conduit
%{_datadir}/applications/conduit.desktop
%config(noreplace) %{_sysconfdir}/apparmor.d/abstractions/conduit

%changelog
* Sat Oct 03 2026 Ole Algoritme <olealgoritme@gmail.com> - 0.1.0-1
- Initial package
