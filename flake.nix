{
  description = "Wicki-Hayden isomorphic MIDI controller from a laptop keyboard";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { nixpkgs, ... }:
    let
      pkgs = nixpkgs.legacyPackages.x86_64-linux;
      lib = pkgs.lib;
      soundfont = "${pkgs.soundfont-fluid}/share/soundfonts/FluidR3_GM2-2.sf2";

      isomorphic = pkgs.rustPlatform.buildRustPackage {
        pname = "isomorphic";
        version = "0.1.0";
        src = lib.fileset.toSource {
          root = ./.;
          fileset = lib.fileset.unions [ ./Cargo.toml ./Cargo.lock ./src ];
        };
        cargoLock.lockFile = ./Cargo.lock;
        nativeBuildInputs = [ pkgs.pkg-config ];
        buildInputs = [ pkgs.alsa-lib ];
      };

      # `nix run . -- <isomorphic args>`: starts a fluidsynth of its own and
      # plays into it.
      play = pkgs.writeShellApplication {
        name = "isomorphic-play";
        runtimeInputs = [ isomorphic pkgs.fluidsynth pkgs.alsa-utils pkgs.util-linux ];
        text = ''
          # fluidsynth only stays up while its command shell is reading, and
          # spins at full CPU once that shell hits EOF, so its stdin is a pipe
          # we hold open (fd synth_in) and never write to. --pdeathsig stops it
          # whenever this script exits, however that happens.
          # shellcheck disable=SC2034
          exec {synth_in}> >(exec setpriv --pdeathsig TERM \
            fluidsynth -a pulseaudio -m alsa_seq ${soundfont} >/dev/null)
          synth_pid=$!
          # ALSA names fluidsynth's sequencer client after its pid, which
          # tells our synth apart from any other running one.
          synth="FLUID Synth ($synth_pid)"

          for _ in $(seq 100); do
            if aconnect -o | grep -qF "'$synth'"; then
              isomorphic --synth "$synth" "$@"
              exit
            fi
            if ! kill -0 "$synth_pid" 2>/dev/null; then
              echo "error: fluidsynth exited before opening its MIDI port" >&2
              exit 1
            fi
            sleep 0.1
          done
          echo "error: fluidsynth didn't open its MIDI port within 10 s" >&2
          exit 1
        '';
      };
    in {
      packages.x86_64-linux = {
        inherit isomorphic;
        default = play;
      };

      # Rust itself comes from the systemwide toolchain. alsa-lib is for
      # midir's ALSA backend; fluidsynth and $SOUNDFONT are a synth to play through;
      # alsa-utils provides aconnect for wiring them together.
      devShells.x86_64-linux.default = pkgs.mkShell {
        nativeBuildInputs = [ pkgs.pkg-config ];
        buildInputs = [ pkgs.alsa-lib pkgs.fluidsynth pkgs.alsa-utils ];
        SOUNDFONT = soundfont;
      };
    };
}
