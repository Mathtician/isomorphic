//! Plays a laptop keyboard as a Wicki-Hayden isomorphic MIDI controller.
//!
//! On a standard keyboard each alphanumeric row sits roughly half a key to the
//! right of the row above it, so the four rows form a (sheared) hex grid: the
//! key at column `c` of one row has column `c` of the row above as its up-left
//! neighbour and column `c + 1` as its up-right neighbour. Wicki-Hayden assigns
//! +2 semitones to a step right and +5 to a step up-left, which makes up-right
//! +7. So the note of a key is simply `base + 2*col + 5*row`.
//!
//! Input is read straight from evdev (physical key positions, true key-up
//! events, no autorepeat) with the device grabbed so keystrokes don't leak into
//! other programs. Output is a virtual ALSA MIDI port for any synth to connect to.

use std::{collections::HashSet, path::PathBuf, thread, time::Duration};

use clap::Parser;
use evdev::{Device, EventSummary, KeyCode};
use midir::{MidiOutput, MidiOutputConnection, os::unix::VirtualOutput};

/// Physical key rows, bottom to top, each paired with a label for the startup grid.
const ROWS: [&[(KeyCode, &str)]; 4] = [
    &[
        (KeyCode::KEY_Z, "z"),
        (KeyCode::KEY_X, "x"),
        (KeyCode::KEY_C, "c"),
        (KeyCode::KEY_V, "v"),
        (KeyCode::KEY_B, "b"),
        (KeyCode::KEY_N, "n"),
        (KeyCode::KEY_M, "m"),
        (KeyCode::KEY_COMMA, ","),
        (KeyCode::KEY_DOT, "."),
        (KeyCode::KEY_SLASH, "/"),
    ],
    &[
        (KeyCode::KEY_A, "a"),
        (KeyCode::KEY_S, "s"),
        (KeyCode::KEY_D, "d"),
        (KeyCode::KEY_F, "f"),
        (KeyCode::KEY_G, "g"),
        (KeyCode::KEY_H, "h"),
        (KeyCode::KEY_J, "j"),
        (KeyCode::KEY_K, "k"),
        (KeyCode::KEY_L, "l"),
        (KeyCode::KEY_SEMICOLON, ";"),
        (KeyCode::KEY_APOSTROPHE, "'"),
    ],
    &[
        (KeyCode::KEY_Q, "q"),
        (KeyCode::KEY_W, "w"),
        (KeyCode::KEY_E, "e"),
        (KeyCode::KEY_R, "r"),
        (KeyCode::KEY_T, "t"),
        (KeyCode::KEY_Y, "y"),
        (KeyCode::KEY_U, "u"),
        (KeyCode::KEY_I, "i"),
        (KeyCode::KEY_O, "o"),
        (KeyCode::KEY_P, "p"),
        (KeyCode::KEY_LEFTBRACE, "["),
        (KeyCode::KEY_RIGHTBRACE, "]"),
    ],
    &[
        (KeyCode::KEY_1, "1"),
        (KeyCode::KEY_2, "2"),
        (KeyCode::KEY_3, "3"),
        (KeyCode::KEY_4, "4"),
        (KeyCode::KEY_5, "5"),
        (KeyCode::KEY_6, "6"),
        (KeyCode::KEY_7, "7"),
        (KeyCode::KEY_8, "8"),
        (KeyCode::KEY_9, "9"),
        (KeyCode::KEY_0, "0"),
        (KeyCode::KEY_MINUS, "-"),
        (KeyCode::KEY_EQUAL, "="),
    ],
];

/// Semitones per step right (a whole tone).
const RIGHT: u32 = 2;
/// Semitones per step up-left (a perfect fourth); up-right is then RIGHT + UP_LEFT = 7, a fifth.
const UP_LEFT: u32 = 5;

const QUIT_KEY: KeyCode = KeyCode::KEY_ESC;

// evdev key event values.
const RELEASE: i32 = 0;
const PRESS: i32 = 1;
const AUTOREPEAT: i32 = 2;

#[derive(Parser)]
#[command(about = "Play the keyboard as a Wicki-Hayden MIDI controller. Esc quits.")]
struct Args {
    /// evdev keyboard device to read (and grab)
    #[arg(long, default_value = "/dev/input/by-path/platform-i8042-serio-0-event-kbd")]
    device: PathBuf,
    /// MIDI note number of the Z key (48 = C3)
    #[arg(long, default_value_t = 48)]
    base: u8,
    /// MIDI channel
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u8).range(1..=16))]
    channel: u8,
    /// Note-on velocity (laptop keys aren't velocity-sensitive)
    #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u8).range(1..=127))]
    velocity: u8,
    /// Name of the virtual MIDI output port
    #[arg(long, default_value = "isomorphic")]
    port_name: String,
}

/// A MIDI note number; always < 128.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Note(u8);

impl Note {
    fn new(n: u32) -> Option<Note> {
        u8::try_from(n).ok().filter(|&n| n < 128).map(Note)
    }

    fn name(self) -> String {
        const NAMES: [&str; 12] = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];
        // Scientific pitch notation: MIDI 60 is C4.
        format!("{}{}", NAMES[self.0 as usize % 12], self.0 as i32 / 12 - 1)
    }
}

/// Key-to-note assignment, built once at startup. Every key in ROWS gets a valid note.
struct Layout {
    keys: Vec<(KeyCode, Note)>,
}

impl Layout {
    fn new(base: u8) -> Result<Layout, String> {
        let mut keys = Vec::new();
        for (row, keys_in_row) in ROWS.iter().enumerate() {
            for (col, &(key, label)) in keys_in_row.iter().enumerate() {
                let n = base as u32 + RIGHT * col as u32 + UP_LEFT * row as u32;
                let note = Note::new(n).ok_or_else(|| {
                    format!("--base {base} puts key {label:?} at MIDI note {n}, above 127")
                })?;
                keys.push((key, note));
            }
        }
        Ok(Layout { keys })
    }

    fn note(&self, key: KeyCode) -> Option<Note> {
        self.keys.iter().find(|&&(k, _)| k == key).map(|&(_, n)| n)
    }

    fn print_grid(&self) {
        // Each row down is shifted half a cell right, mirroring the physical stagger.
        const CELL: usize = 4;
        for (row, keys_in_row) in ROWS.iter().enumerate().rev() {
            let indent = " ".repeat((ROWS.len() - 1 - row) * CELL / 2);
            let labels: String = keys_in_row.iter().map(|(_, l)| format!("{l:<CELL$}")).collect();
            let notes: String = keys_in_row
                .iter()
                .map(|&(k, _)| format!("{:<CELL$}", self.note(k).unwrap().name()))
                .collect();
            println!("{indent}{labels}\n{indent}{notes}\n");
        }
    }
}

/// Tracks which mapped keys are down and turns key transitions into MIDI.
///
/// Several keys can share a note (the layout repeats pitches). A note sounds
/// exactly while at least one key mapped to it is held: it starts when the
/// first such key goes down and stops when the last one comes up. This is
/// derived from `held` directly rather than kept as a separate counter.
struct Player<'a> {
    layout: &'a Layout,
    out: MidiOutputConnection,
    channel: u8, // 0-based
    velocity: u8,
    held: HashSet<KeyCode>,
}

impl Player<'_> {
    fn is_sounding(&self, note: Note) -> bool {
        self.held.iter().any(|&k| self.layout.note(k) == Some(note))
    }

    fn press(&mut self, key: KeyCode) -> Result<(), midir::SendError> {
        let Some(note) = self.layout.note(key) else { return Ok(()) };
        let was_sounding = self.is_sounding(note);
        if self.held.insert(key) && !was_sounding {
            self.out.send(&[0x90 | self.channel, note.0, self.velocity])?;
        }
        Ok(())
    }

    fn release(&mut self, key: KeyCode) -> Result<(), midir::SendError> {
        let Some(note) = self.layout.note(key) else { return Ok(()) };
        if self.held.remove(&key) && !self.is_sounding(note) {
            self.out.send(&[0x80 | self.channel, note.0, 0])?;
        }
        Ok(())
    }

    /// Silence everything, e.g. on exit or error, so no note is left hanging.
    fn release_all(&mut self) -> Result<(), midir::SendError> {
        for key in self.held.clone() {
            self.release(key)?;
        }
        assert!(self.held.is_empty());
        // All Notes Off, in case the synth disagrees with us about what's sounding.
        self.out.send(&[0xB0 | self.channel, 123, 0])
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let layout = Layout::new(args.base)?;
    layout.print_grid();

    let mut device = Device::open(&args.device)
        .map_err(|e| format!("opening {}: {e} (are you in the `input` group?)", args.device.display()))?;
    let supported = device.supported_keys().ok_or("device reports no keys")?;
    for &(key, _) in layout.keys.iter().chain([&(QUIT_KEY, Note(0))]) {
        if !supported.contains(key) {
            return Err(format!("device {:?} lacks key {key:?}", device.name()).into());
        }
    }

    let out = MidiOutput::new("isomorphic")?
        .create_virtual(&args.port_name)
        .map_err(|e| format!("creating MIDI port: {e}"))?;

    println!("Reading {:?}; MIDI port {:?}.", device.name().unwrap_or("?"), args.port_name);

    // Grabbing while a key is down (e.g. the Enter that launched us) would
    // swallow its release, leaving the terminal autorepeating it forever.
    if device.get_key_state()?.iter().next().is_some() {
        println!("Waiting for all keys to be released...");
        while device.get_key_state()?.iter().next().is_some() {
            thread::sleep(Duration::from_millis(10));
        }
    }
    device.grab()?;
    println!("Keyboard grabbed. Play! Esc quits.");

    let mut player = Player {
        layout: &layout,
        out,
        channel: args.channel - 1,
        velocity: args.velocity,
        held: HashSet::new(),
    };
    let result = play(&mut device, &mut player);
    player.release_all()?;
    device.ungrab()?;
    result
}

/// Runs until the quit key is pressed or an error occurs.
fn play(device: &mut Device, player: &mut Player) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        for event in device.fetch_events()? {
            let EventSummary::Key(_, key, value) = event.destructure() else { continue };
            match value {
                PRESS if key == QUIT_KEY => return Ok(()),
                PRESS => player.press(key)?,
                RELEASE => player.release(key)?,
                AUTOREPEAT => {}
                v => return Err(format!("unexpected key event value {v} for {key:?}").into()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note_of(layout: &Layout, key: KeyCode) -> u8 {
        layout.note(key).unwrap().0
    }

    #[test]
    fn wicki_hayden_intervals() {
        let l = Layout::new(48).unwrap();
        assert_eq!(note_of(&l, KeyCode::KEY_Z), 48); // C3
        assert_eq!(note_of(&l, KeyCode::KEY_X), 50); // right: +2
        assert_eq!(note_of(&l, KeyCode::KEY_A), 53); // up-left: +5
        assert_eq!(note_of(&l, KeyCode::KEY_S), 55); // up-right: +7
        // C major triad: Z, C, S.
        assert_eq!(note_of(&l, KeyCode::KEY_C), 52);
        assert_eq!(note_of(&l, KeyCode::KEY_EQUAL), 48 + 2 * 11 + 5 * 3);
    }

    #[test]
    fn all_keys_distinct() {
        let l = Layout::new(0).unwrap();
        let set: HashSet<_> = l.keys.iter().map(|&(k, _)| k).collect();
        assert_eq!(set.len(), l.keys.len());
    }

    #[test]
    fn base_range() {
        assert!(Layout::new(127 - 37).is_ok());
        assert!(Layout::new(127 - 36).is_err());
    }

    #[test]
    fn note_names() {
        assert_eq!(Note(60).name(), "C4");
        assert_eq!(Note(0).name(), "C-1");
        assert_eq!(Note(70).name(), "A#4");
    }
}
