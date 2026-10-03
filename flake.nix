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
  # ($out/bin/{conduit,conduit-backend,conduit-viewer,conduit-vmm,qemu-system-x86_64}),
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
      viewerBin = "conduit-viewer";
      vmmBin = "conduit-vmm";

      # nixpkgs' Rust is used rather than the rust-toolchain.toml pin (1.90.0);
      # nixos-unstable is newer, and the code needs nothing beyond stable.
      rustPkg = args: pkgs.rustPlatform.buildRustPackage ({
        inherit version;
        doCheck = false; # CI runs the tests; some need /dev/nvidiactl
      } // args);

      backend = rustPkg {
        pname = "conduit-backend";
        src = ./host/backend;
        cargoLock.lockFile = ./host/backend/Cargo.lock;
        cargoLock.allowBuiltinFetchGit = true;
        buildFeatures = [ "vhost-user" ];
        cargoBuildFlags = [ "-p" "device" "--bin" backendBin "--bin" userspaceBin ];
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
        ln -s ${backend}/bin/conduit-backend $out/bin/conduit-backend
        ln -s ${backend}/bin/conduit-userspace $out/bin/conduit-userspace
        ln -s ${vmm}/bin/conduit-vmm         $out/bin/conduit-vmm
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
        inherit conduit backend viewer vmm cli qemu;
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
