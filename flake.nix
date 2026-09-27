{
  description = "Wicki-Hayden isomorphic MIDI controller from a laptop keyboard";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { nixpkgs, ... }:
    let
      pkgs = nixpkgs.legacyPackages.x86_64-linux;
    in {
      # Rust itself comes from the systemwide toolchain. alsa-lib is for
      # midir's ALSA backend; fluidsynth and $SOUNDFONT are a synth to play through.
      devShells.x86_64-linux.default = pkgs.mkShell {
        nativeBuildInputs = [ pkgs.pkg-config ];
        buildInputs = [ pkgs.alsa-lib pkgs.fluidsynth ];
        SOUNDFONT = "${pkgs.soundfont-fluid}/share/soundfonts/FluidR3_GM2-2.sf2";
      };
    };
}
