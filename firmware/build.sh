#!/usr/bin/env bash
# Builds the instrument firmware and, as a fallback to flash, Keychron's stock
# keymap from the same source. Run inside `nix develop` in this directory.
set -euo pipefail
cd "$(dirname "$0")"

# The Keychron commit instrument.c was written against (branch 2025q3).
KEYCHRON_COMMIT=9ada9b7baecb9591c469b9b068146ac5891a480a

if [ ! -d qmk_firmware ]; then
    git init -q qmk_firmware
    git -C qmk_firmware remote add origin https://github.com/Keychron/qmk_firmware.git
    git -C qmk_firmware fetch -q --depth 1 --filter=blob:none origin "$KEYCHRON_COMMIT"
    git -C qmk_firmware checkout -q FETCH_HEAD
    git -C qmk_firmware submodule update -q --init --depth 1 --recommend-shallow
fi
actual=$(git -C qmk_firmware rev-parse HEAD)
if [ "$actual" != "$KEYCHRON_COMMIT" ]; then
    echo "qmk_firmware is at $actual, expected $KEYCHRON_COMMIT" >&2
    exit 1
fi

ln -sfn "$PWD/keymap" qmk_firmware/keyboards/keychron/q1_he/ansi_encoder/keymaps/isomorphic

make -C test
cd qmk_firmware
qmk compile -kb keychron/q1_he/ansi_encoder -km isomorphic
qmk compile -kb keychron/q1_he/ansi_encoder -km keychron
ls -l keychron_q1_he_ansi_encoder_{isomorphic,keychron}.bin
