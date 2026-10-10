# Conduit BIOS version pin. Sourced by build.sh, the packaging and CI.
# Bump CONDUIT_BIOS_REV when the logo or build script changes; bump the edk2
# pin to follow Ubuntu's ovmf package (the vars layout must stay compatible).
EDK2_UPSTREAM=2024.02
EDK2_DEB_VERSION=2024.02-2ubuntu0.9
EDK2_ORIG_SHA256=3986e42620845cf799ed2cc863fe97603a433e64842e335fe8f4746d9b3b5b21
EDK2_DEBIAN_SHA256=884e5d43152d3f5a1aea15b79fe196818d61615734942f24d0fb42c80f6bf6c7
CONDUIT_BIOS_REV=1
CONDUIT_BIOS_VERSION="${EDK2_UPSTREAM}.${EDK2_DEB_VERSION#*-}.${CONDUIT_BIOS_REV}"
