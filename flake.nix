{
  # Conduit on Nix / NixOS.
  #
  #   nix run github:olealgoritme/conduit -- view myvm     # apps.default
  #   nix build github:olealgoritme/conduit                # packages.default
  #
  # NixOS guests: import nixosModules.guest to build the guest module for the
  # VM's kernel. The host-side AppArmor/SELinux hookup done by the distro
  # packages does not apply on NixOS (libvirt there uses neither by default).
  #
  # Layout: packages.default is a prefix that mirrors /opt/conduit
  # ($out/bin/{conduit,conduit-backend,conduit-stream,conduit-venus,conduit-viewer,conduit-vmm,qemu-system-x86_64}),
  # and `conduit` is wrapped with CONDUIT_PREFIX=$out so it finds its helpers
  # there instead of /opt/conduit (see docs/PACKAGING.md, "CLI contract").
  description = "Conduit: share your NVIDIA GPU with a VM and see its desktop on yours";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
      lib = pkgs.lib;

      version = "${(builtins.fromTOML (builtins.readFile ./Cargo.toml)).workspace.package.version}+${self.shortRev or "dirty"}";

      # Cargo/make binary names (same variables as packaging/build.sh).
      backendBin = "conduit-backend";
      userspaceBin = "conduit-userspace";
      streamBin = "conduit-stream";
      viewerBin = "conduit-viewer";
      vmmBin = "conduit-vmm";

      # nixpkgs' Rust is used rather than the rust-toolchain.toml pin (1.90.0);
      # nixos-unstable is newer, and the code needs nothing beyond stable.
      rustPkg = args: pkgs.rustPlatform.buildRustPackage ({
        inherit version;
        doCheck = false; # CI runs the tests; some need /dev/nvidiactl
      } // args);

      # conduit-venus's Rust sources (host/venus without its submodules): the
      # backend's `venus` feature uses the crate (no virglrenderer), the
      # renderer below builds its binary.
      venusCrate = [
        ./host/venus/Cargo.toml
        ./host/venus/Cargo.lock
        ./host/venus/build.rs
        ./host/venus/src
        ./host/venus/examples
      ];

      # Same features as packaging/build.sh (BACKEND_FEATURES). The `venus`
      # feature pulls in ../../venus, so the source is host/ with only the
      # backend and the venus crate in it.
      backend = rustPkg {
        pname = "conduit-backend";
        src = lib.fileset.toSource {
          root = ./host;
          fileset = lib.fileset.unions ([ ./host/backend ] ++ venusCrate);
        };
        cargoRoot = "backend";
        buildAndTestSubdir = "backend";
        cargoLock.lockFile = ./host/backend/Cargo.lock;
        cargoLock.allowBuiltinFetchGit = true;
        buildFeatures = [ "vhost-user" "venus" ];
        cargoBuildFlags = [ "-p" "device" "--bin" backendBin "--bin" userspaceBin ];
      };

      # The Venus renderer for Windows guests (docs/VENUS.md): conduit-venus
      # and the virglrenderer it links, Venus only, built like
      # host/venus/build-virglrenderer.sh does. Flake sources leave submodules
      # out, so the two it is built from are fetched at the revisions
      # host/venus/third_party/ pins (`git submodule status host/venus`; CI
      # checks that they stay in step).
      venusProtocol = pkgs.stdenv.mkDerivation {
        pname = "venus-protocol";
        version = "fe08e82";
        src = builtins.fetchGit {
          url = "https://github.com/winboat-org/venus-protocol.git";
          rev = "fe08e82c3819e8ee3c547b1ea810fde61f46fa78";
          allRefs = true;
        };
        nativeBuildInputs = with pkgs; [ meson ninja (python3.withPackages (p: [ p.mako ])) ];
        mesonFlags = [ "-Dwerror=false" ];
      };

      venusPatches = lib.optionals (builtins.pathExists ./host/venus/patches)
        (map (n: ./host/venus/patches + "/${n}")
          (lib.sort lib.lessThan
            (builtins.filter (lib.hasSuffix ".patch")
              (builtins.attrNames (builtins.readDir ./host/venus/patches)))));

      virglrenderer = pkgs.stdenv.mkDerivation {
        pname = "virglrenderer-venus";
        version = "aafa9bd";
        src = builtins.fetchGit {
          url = "https://gitlab.freedesktop.org/virgl/virglrenderer.git";
          rev = "aafa9bd234a43c31004ec768ce000b21cf7b99ca";
          allRefs = true;
        };
        patches = venusPatches;
        postPatch = "patchShebangs .";
        nativeBuildInputs = with pkgs; [
          meson
          ninja
          pkg-config
          (python3.withPackages (p: [ p.mako p.pyyaml ]))
        ];
        buildInputs = with pkgs; [ libdrm vulkan-headers vulkan-loader venusProtocol ];
        mesonBuildType = "release";
        mesonFlags = [
          "-Dvenus=true"
          "-Dvrend=false"
          "-Dvideo=false"
          "-Drender-server-mode=thread"
          "-Drender-server-worker=thread"
          "-Dtests=false"
        ];
        # virglrenderer dlopen()s libvulkan.so.1, which is searched for on
        # its own RUNPATH (after fixup, which drops unused entries); the
        # loader then finds the driver's ICD (/run/opengl-driver on NixOS).
        postFixup = ''
          patchelf --add-rpath ${pkgs.vulkan-loader}/lib $out/lib/libvirglrenderer.so.1
        '';
      };

      # build.rs finds virglrenderer through pkg-config and puts its lib/ on
      # the binary's RUNPATH.
      venus = rustPkg {
        pname = "conduit-venus";
        src = lib.fileset.toSource {
          root = ./host/venus;
          fileset = lib.fileset.unions venusCrate;
        };
        cargoLock.lockFile = ./host/venus/Cargo.lock;
        nativeBuildInputs = [ pkgs.pkg-config ];
        buildInputs = [ virglrenderer ];
        buildFeatures = [ "renderer" ];
        cargoBuildFlags = [ "--bin" "conduit-venus" ];
      };

      vmm = rustPkg {
        pname = "conduit-vmm";
        src = ./host/vmm;
        cargoLock.lockFile = ./host/vmm/Cargo.lock;
        # rutabaga_gfx is a git dependency (unused without default features,
        # but still in Cargo.lock); builtins.fetchGit avoids a hash per rev.
        cargoLock.allowBuiltinFetchGit = true;
        buildNoDefaultFeatures = true;
        cargoBuildFlags = [ "--bin" vmmBin ];
      };

      # cli/ is a member of the workspace at the repo root.
      cli = rustPkg {
        pname = "conduit-cli";
        src = lib.fileset.toSource {
          root = ./.;
          fileset = lib.fileset.unions [ ./Cargo.toml ./Cargo.lock ./cli ];
        };
        cargoLock.lockFile = ./Cargo.lock;
        cargoBuildFlags = [ "-p" "conduit" ];
      };

      # The network stream host (docs/STREAMING.md): C half needs EGL and GBM
      # headers (pkg-config), the Rust half OpenSSL. NVENC and CUDA are
      # dlopen()ed from the driver, so the binary gets the driver runpath
      # (/run/opengl-driver/lib on NixOS).
      stream = rustPkg {
        pname = "conduit-stream";
        src = ./host/stream;
        cargoLock.lockFile = ./host/stream/Cargo.lock;
        nativeBuildInputs = with pkgs; [
          pkg-config
          addDriverRunpath
        ];
        buildInputs = with pkgs; [
          libGL
          (pkgs.libgbm or pkgs.mesa)
          openssl
        ];
        cargoBuildFlags = [ "--bin" streamBin ];
        postFixup = ''
          addDriverRunpath $out/bin/${streamBin}
        '';
      };

      viewer = pkgs.stdenv.mkDerivation {
        pname = "conduit-viewer";
        inherit version;
        src = ./host/viewer;
        nativeBuildInputs = with pkgs; [ pkg-config wayland-scanner python3 ];
        buildInputs = with pkgs; [
          wayland
          wayland-protocols
          (pkgs.libxcb or pkgs.xorg.libxcb)
          (pkgs.libgbm or pkgs.mesa)
        ];
        makeFlags = [ "all" ];
        enableParallelBuilding = true;
        installPhase = ''
          runHook preInstall
          install -Dm755 ${viewerBin} $out/bin/conduit-viewer
          runHook postInstall
        '';
      };

      # QEMU >= 11.1 (vhost-user shared memory) plus Conduit's patches.
      qemuPatches = lib.optionals (builtins.pathExists ./host/qemu/patches)
        (map (n: ./host/qemu/patches + "/${n}")
          (lib.sort lib.lessThan
            (builtins.filter (lib.hasSuffix ".patch")
              (builtins.attrNames (builtins.readDir ./host/qemu/patches)))));
      qemu =
        let base = pkgs.qemu_kvm; in
        if lib.versionAtLeast base.version "11.1" then
          base.overrideAttrs (o: { patches = (o.patches or [ ]) ++ qemuPatches; })
        else
          base.overrideAttrs (o: {
            # Same release and checksum as host/qemu/build-qemu.sh.
            version = "11.1.2";
            src = pkgs.fetchurl {
              url = "https://download.qemu.org/qemu-11.1.2.tar.xz";
              sha256 = "731b5681e4bb18be313231579b8efd0296c5b015fa36dc533874b639ba838016";
            };
            # nixpkgs' patches target its own QEMU version; keep only ours.
            patches = qemuPatches;
          });

      conduit = pkgs.runCommand "conduit-${version}"
        {
          nativeBuildInputs = [ pkgs.makeWrapper ];
          meta = {
            description = "Share your NVIDIA GPU with a VM and see its desktop on yours";
            homepage = "https://github.com/olealgoritme/conduit";
            license = with lib.licenses; [ asl20 bsd3 gpl2Only ];
            platforms = [ system ];
            mainProgram = "conduit";
          };
        } ''
        mkdir -p $out/bin $out/share/applications
        # The app icon set (Icon=conduit): the same installer the packages use.
        . ${./packaging/common/icons.sh}
        ICON_SRC=${./packaging/common/icons} install_icons $out
        ln -s ${backend}/bin/conduit-backend $out/bin/conduit-backend
        ln -s ${backend}/bin/conduit-userspace $out/bin/conduit-userspace
        ln -s ${vmm}/bin/conduit-vmm         $out/bin/conduit-vmm
        ln -s ${stream}/bin/conduit-stream   $out/bin/conduit-stream
        ln -s ${venus}/bin/conduit-venus     $out/bin/conduit-venus
        ln -s ${viewer}/bin/conduit-viewer   $out/bin/conduit-viewer
        ln -s ${qemu}/bin/qemu-system-x86_64 $out/bin/qemu-system-x86_64
        makeWrapper ${cli}/bin/conduit $out/bin/conduit \
          --set-default CONDUIT_PREFIX $out
        substitute ${./packaging/common/conduit.desktop} \
          $out/share/applications/conduit.desktop \
          --replace-fail "Exec=conduit" "Exec=$out/bin/conduit" \
          --replace-fail "TryExec=conduit" "TryExec=$out/bin/conduit"
      '';

      # Guest kernel module for a given kernel (needs Linux >= 6.4).
      mkGuestModule = kernel: pkgs.stdenv.mkDerivation {
        pname = "conduit-guest";
        version = "${version}-${kernel.modDirVersion}";
        src = ./guest/linux;
        nativeBuildInputs = kernel.moduleBuildDependencies;
        buildPhase = ''
          runHook preBuild
          make -C ${kernel.dev}/lib/modules/${kernel.modDirVersion}/build \
            M=$PWD CONFIG_CONDUIT_GPU=m modules
          runHook postBuild
        '';
        installPhase = ''
          runHook preInstall
          install -Dm644 conduit_gpu.ko \
            $out/lib/modules/${kernel.modDirVersion}/updates/conduit_gpu.ko
          runHook postInstall
        '';
        meta.license = lib.licenses.gpl2Only;
        meta.broken = lib.versionOlder kernel.version "6.4";
      };
    in
    {
      packages.${system} = {
        default = conduit;
        inherit conduit backend stream venus virglrenderer viewer vmm cli qemu;
        conduit-guest = mkGuestModule pkgs.linuxPackages_latest.kernel;
      };

      apps.${system}.default = {
        type = "app";
        program = "${conduit}/bin/conduit";
      };

      lib.mkGuestModule = mkGuestModule;

      # Inside a NixOS VM: imports = [ conduit.nixosModules.guest ];
      nixosModules.guest = { config, ... }: {
        boot.extraModulePackages = [ (mkGuestModule config.boot.kernelPackages.kernel) ];
        boot.kernelModules = [ "conduit_gpu" ];
      };

      devShells.${system}.default = pkgs.mkShell {
        inputsFrom = [ viewer ];
        packages = with pkgs; [ rustup nfpm python3 meson ninja dtc patchelf ];
      };
    };
}
