{
  description = "86slopper: x86-32 PC emulator for Apple Silicon";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/0bb2eedc20e4739f29e8045034f2828ad7054f8f";

  outputs =
    { nixpkgs, ... }:
    let
      system = "aarch64-darwin";
      pkgs = nixpkgs.legacyPackages.${system};
      x86pkgs = nixpkgs.legacyPackages.x86_64-linux;
    in
    {
      packages.${system} = {
        seabios = (x86pkgs.seabios.override { ___build-type = "qemu"; }).overrideAttrs (old: {
          patches = (old.patches or [ ]) ++ [ ./nix/seabios-i440bx.patch ];
          postConfigure = ''
            cat > .config <<CONFIG
            CONFIG_QEMU=y
            CONFIG_QEMU_HARDWARE=y
            CONFIG_PERMIT_UNALIGNED_PCIROM=y
            CONFIG
            make olddefconfig
          '';
        });

        seavgabios = (x86pkgs.seabios.override { ___build-type = "qemu"; }).overrideAttrs {
          pname = "seavgabios-stdvga";
          postConfigure = ''
            cat > .config <<CONFIG
            CONFIG_QEMU=y
            CONFIG_VGA_BOCHS=y
            CONFIG_VGA_BOCHS_STDVGA=y
            CONFIG_VGA_PCI=y
            CONFIG_OVERRIDE_PCI_ID=n
            CONFIG
            make olddefconfig
          '';
          installPhase = ''
            runHook preInstall
            mkdir -p $doc
            install -Dm644 out/vgabios.bin -t $out/share/seabios/
            runHook postInstall
          '';
        };
      };

      devShells.${system}.default = pkgs.mkShellNoCC {
        packages = [
          pkgs.acpica-tools
          pkgs.nasm
          pkgs.pkg-config
        ];
        buildInputs = [
          pkgs.glib
          pkgs.libslirp
        ];
      };
    };
}
