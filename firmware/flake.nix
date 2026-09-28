{
  description = "Build and flash the isomorphic firmware for the Keychron Q1 HE";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { nixpkgs, ... }:
    let
      pkgs = nixpkgs.legacyPackages.x86_64-linux;
    in {
      devShells.x86_64-linux.default = pkgs.mkShell {
        # qmk brings the CLI plus the ARM toolchain and dfu-util; gcc is for
        # unit-testing the platform-independent instrument logic on the host.
        packages = [ pkgs.qmk pkgs.dfu-util pkgs.gcc ];
        # Relative to where `nix develop` runs (this directory); a path here
        # would point into the Nix store copy of the flake.
        shellHook = ''
          export QMK_HOME="$PWD/qmk_firmware"
        '';
      };
    };
}
