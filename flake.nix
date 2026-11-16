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
        seabios = x86pkgs.seabios.override { ___build-type = "qemu"; };
      };

      devShells.${system}.default = pkgs.mkShellNoCC {
        packages = [
          pkgs.acpica-tools
          pkgs.nasm
        ];
      };
    };
}
