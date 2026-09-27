{
  description = "Wicki-Hayden isomorphic MIDI controller from a laptop keyboard";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { nixpkgs, ... }:
    let
      pkgs = nixpkgs.legacyPackages.x86_64-linux;
    in {
      # Rust itself comes from the systemwide toolchain; this only supplies
      # the native libraries midir's ALSA backend links against.
      devShells.x86_64-linux.default = pkgs.mkShell {
        nativeBuildInputs = [ pkgs.pkg-config ];
        buildInputs = [ pkgs.alsa-lib ];
      };
    };
}
