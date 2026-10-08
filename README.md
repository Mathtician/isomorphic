# isomorphic

Play a computer keyboard as a [Wicki-Hayden](https://en.wikipedia.org/wiki/Wicki-Hayden_note_layout)
isomorphic MIDI controller.

Try the layout in your browser: <https://mathtician.github.io/isomorphic/>

## Layout

Each alphanumeric row of a standard keyboard sits about half a key to the right
of the row above, so the four rows form a sheared hex grid. Wicki-Hayden gives
+2 semitones for a step right and +5 for a step up-left (so +7 up-right), which
makes the note of a key simply

    note = base + 2·col + 5·row

with `row` counted from the bottom (Z row = 0) and `base` the MIDI note of Z
(`--base`, default 48 = C3).

Controls: `` ` `` and `1` shift octaves, Shift works the sustain pedal (CC 64),
Esc quits.

## Inputs

- `--input laptop` (default): the laptop keyboard via evdev, fixed velocity.
- `--input keychron-stock`: a Keychron Q1 HE (ANSI) on stock firmware; velocity
  comes from polling analog key travel over raw HID.
- `--input keychron`: a Keychron Q1 HE running the instrument firmware in
  [`firmware/`](firmware/), which measures velocity and pressure on the
  keyboard itself and lights the keys of the chosen scale (`--key`).

The keyboard is grabbed exclusively while playing, so you need read access to
the evdev device (and the hidraw device for the Keychron modes), e.g. via the
`input` group or a udev rule.

Output goes to a synth over ALSA MIDI (`--synth`, default a port named like
`FLUID`), or to a virtual port (`--virtual-port NAME`) you wire up yourself.
`--record FILE.mid` saves everything sent to the synth as a Standard MIDI File.
See `--help` for the velocity tuning options.

## Running

With Nix (x86_64-linux), this starts its own fluidsynth and plays into it:

    nix run github:Mathtician/isomorphic -- --input laptop

For development, `nix develop` provides alsa-lib, fluidsynth (with
`$SOUNDFONT`) and alsa-utils; Rust comes from your own toolchain:

    nix develop
    fluidsynth -a pulseaudio -m alsa_seq "$SOUNDFONT" &
    cargo run --release -- --input laptop

`cargo run --bin probe` is a small tool for checking the Keychron's analog
travel polling.

## Keychron Q1 HE firmware

`firmware/` holds only this project's own keymap (`firmware/keymap/`), not a
QMK checkout. `firmware/build.sh` (run inside `nix develop` in `firmware/`)
fetches Keychron's [qmk_firmware](https://github.com/Keychron/qmk_firmware) at
a pinned commit (branch `2025q3`, `9ada9b7`), links the keymap in, runs the
host-side unit tests of the platform-independent instrument logic
(`firmware/test/`), and builds both the instrument firmware and Keychron's stock
keymap (as a fallback to flash back).

The instrument protocol (VIA custom-value commands host → keyboard, raw HID
event reports keyboard → host) is documented at the top of
`firmware/keymap/instrument.c`.

**License note:** `firmware/keymap/keymap.c` is Keychron's stock keymap,
copyright Keychron and licensed GPL-2.0-or-later (notice kept in the file), and
the firmware is built against QMK, which is GPL. Firmware built from this
directory is therefore a GPL work.
