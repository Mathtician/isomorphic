//! Plays a keyboard as a Wicki-Hayden isomorphic MIDI controller.
//!
//! On a standard keyboard each alphanumeric row sits roughly half a key to the
//! right of the row above it, so the four rows form a (sheared) hex grid: the
//! key at column `c` of one row has column `c` of the row above as its up-left
//! neighbour and column `c + 1` as its up-right neighbour. Wicki-Hayden assigns
//! +2 semitones to a step right and +5 to a step up-left, which makes up-right
//! +7. So the note of a key is simply `base + 2*col + 5*row`.
//!
//! Keys come from one of three inputs (see `input`): the laptop keyboard via
//! evdev at a fixed velocity; a Keychron Q1 HE on stock firmware, with velocity
//! from polling its analog key travel; or a Keychron running our instrument
//! firmware, which measures velocity and pressure itself. Output goes to a synth
//! over ALSA MIDI.

mod input;
mod recording;

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    time::Duration,
};

use clap::{Parser, ValueEnum};
use evdev::KeyCode;
use input::{Evdev, EvdevVelocity, Firmware, Input, Source};
use isomorphic::{
    instrument::{self, Colors},
    keychron::{self, Travel},
    velocity::Curve,
};
use midir::{MidiOutput, MidiOutputConnection, os::unix::VirtualOutput};

/// Physical key rows, bottom to top, each paired with a label for the startup grid.
/// Each row is `(column of its first key, keys)`; a nonzero first column leaves
/// a hole so the remaining keys keep their place in the grid.
const ROWS: [(usize, &[(KeyCode, &str)]); 4] = [
    (
        0,
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
    ),
    (
        0,
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
    ),
    (
        0,
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
    ),
    // 1 is OCTAVE_UP instead of a note.
    (
        1,
        &[
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
    ),
];

/// Semitones per step right (a whole tone).
const RIGHT: u32 = 2;
/// Semitones per step up-left (a perfect fourth); up-right is then RIGHT + UP_LEFT = 7, a fifth.
const UP_LEFT: u32 = 5;

const QUIT_KEY: KeyCode = KeyCode::KEY_ESC;
const OCTAVE_DOWN: KeyCode = KeyCode::KEY_GRAVE;
const OCTAVE_UP: KeyCode = KeyCode::KEY_1;
/// Either Shift works the sustain pedal; it's down while at least one is held.
const SUSTAIN_KEYS: [KeyCode; 2] = [KeyCode::KEY_LEFTSHIFT, KeyCode::KEY_RIGHTSHIFT];
const CONTROL_KEYS: [KeyCode; 5] = [
    QUIT_KEY,
    OCTAVE_DOWN,
    OCTAVE_UP,
    SUSTAIN_KEYS[0],
    SUSTAIN_KEYS[1],
];

/// Key travel at full depth, in 1/60 mm (4.0 mm).
const FULL_TRAVEL: Travel = 240;

/// Instrument-firmware key colours (palette indices 1..=3; 0 is unlit).
const DIATONIC: u8 = 1;
const CHROMATIC: u8 = 2;
const CONTROL: u8 = 3;
const PALETTE: [[u8; 3]; 3] = [
    [255, 190, 120], // diatonic: warm white
    [30, 60, 255],   // chromatic: blue
    [255, 30, 0],    // control: red
];
/// Pitch classes of a major scale, relative to its tonic.
const MAJOR_SCALE: [u8; 7] = [0, 2, 4, 5, 7, 9, 11];

#[derive(Clone, Copy, PartialEq, Eq, Debug, ValueEnum)]
enum InputKind {
    /// The laptop keyboard, at a fixed velocity
    Laptop,
    /// A Keychron Q1 HE on stock firmware, with velocity from polling key travel
    KeychronStock,
    /// A Keychron Q1 HE running the instrument firmware (see firmware/)
    Keychron,
}

#[derive(Parser)]
#[command(
    about = "Play the keyboard as a Wicki-Hayden MIDI controller. ` and 1 shift octaves; Shift sustains; Esc quits."
)]
struct Args {
    /// Which keyboard to play
    #[arg(long, value_enum, default_value_t = InputKind::Laptop)]
    input: InputKind,
    /// evdev keyboard device to read and grab [default: the laptop keyboard,
    /// or the Keychron's for keychron-stock]
    #[arg(long)]
    device: Option<PathBuf>,
    /// For laptop input: note-on velocity
    #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u8).range(1..=127))]
    velocity: u8,
    /// Keychron: depth at which a key sounds, in 1/60 mm (240 = full travel).
    /// For keychron-stock it must be deeper than the actuation point set in
    /// Keychron Launcher.
    #[arg(long, default_value_t = 180, value_parser = clap::value_parser!(u8).range(2..=230))]
    strike_depth: u8,
    /// keychron: depth where descent timing starts (1/60 mm); a key must rise
    /// above it again before it can strike again
    #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u8).range(1..=229))]
    start_depth: u8,
    /// keychron: a sounding key's note stops when it rises above this depth (1/60 mm)
    #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u8).range(1..=230))]
    release_depth: u8,
    /// keychron: smallest travel change (1/60 mm) reported as new pressure
    #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u8).range(1..=255))]
    pressure_step: u8,
    /// Keychron: descent time (ms) at or below which velocity is 127
    #[arg(long, default_value_t = 2.0)]
    fast_ms: f64,
    /// Keychron: descent time (ms) at or above which velocity is --min-velocity
    #[arg(long, default_value_t = 40.0)]
    slow_ms: f64,
    /// Keychron: velocity of the slowest presses
    #[arg(long, default_value_t = 32, value_parser = clap::value_parser!(u8).range(1..=127))]
    min_velocity: u8,
    /// Keychron: print each note's descent time and velocity, for tuning
    /// --fast-ms and --slow-ms
    #[arg(long)]
    show_timing: bool,
    /// keychron: major key whose scale lights up as diatonic (e.g. C, G, Eb, F#)
    #[arg(long, default_value = "C", value_parser = parse_pitch_class)]
    key: u8,
    /// MIDI note number of the Z key (48 = C3)
    #[arg(long, default_value_t = 48)]
    base: u8,
    /// MIDI channel
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u8).range(1..=16))]
    channel: u8,
    /// Connect directly to the one MIDI input port whose name contains this
    /// (e.g. "FLUID" for fluidsynth). The connection is made before the
    /// keyboard is grabbed, since afterwards you can't type anywhere else.
    #[arg(long, default_value = "FLUID", conflicts_with = "virtual_port")]
    synth: String,
    /// Instead of connecting to a synth, create a virtual MIDI port with this
    /// name and leave the wiring to you (e.g. with the mouse in qpwgraph).
    #[arg(long)]
    virtual_port: Option<String>,
    /// Also record everything sent to the synth to this Standard MIDI File
    /// (written on exit; won't overwrite an existing file)
    #[arg(long)]
    record: Option<PathBuf>,
}

/// Parses a note name like "C", "F#" or "Eb" into a pitch class, 0 = C.
fn parse_pitch_class(s: &str) -> Result<u8, String> {
    let mut chars = s.chars();
    let base: i32 = match chars.next().map(|c| c.to_ascii_uppercase()) {
        Some('C') => 0,
        Some('D') => 2,
        Some('E') => 4,
        Some('F') => 5,
        Some('G') => 7,
        Some('A') => 9,
        Some('B') => 11,
        _ => return Err(format!("{s:?} isn't a note name like C, F# or Eb")),
    };
    let accidental = match chars.as_str() {
        "" => 0,
        "#" => 1,
        "b" => -1,
        _ => return Err(format!("{s:?} isn't a note name like C, F# or Eb")),
    };
    Ok((base + accidental).rem_euclid(12) as u8)
}

/// Aftertouch for a struck key's travel: 0 at the strike depth, 127 at full travel.
fn aftertouch(travel: Travel, strike_depth: Travel) -> u8 {
    assert!(strike_depth < FULL_TRAVEL);
    let past = travel
        .saturating_sub(strike_depth)
        .min(FULL_TRAVEL - strike_depth);
    ((past as u32 * 127 + (FULL_TRAVEL - strike_depth) as u32 / 2)
        / (FULL_TRAVEL - strike_depth) as u32) as u8
}

/// A MIDI note number; always < 128.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Note(u8);

impl Note {
    fn new(n: u32) -> Option<Note> {
        u8::try_from(n).ok().filter(|&n| n < 128).map(Note)
    }

    fn transpose(self, semitones: i32) -> Option<Note> {
        u32::try_from(self.0 as i32 + semitones)
            .ok()
            .and_then(Note::new)
    }

    fn name(self) -> String {
        const NAMES: [&str; 12] = [
            "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
        ];
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
        for (row, &(first_col, keys_in_row)) in ROWS.iter().enumerate() {
            for (i, &(key, label)) in keys_in_row.iter().enumerate() {
                let col = first_col + i;
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

    /// Whether every key would still have a valid note after transposing by `semitones`.
    fn transposable(&self, semitones: i32) -> bool {
        self.keys
            .iter()
            .all(|&(_, n)| n.transpose(semitones).is_some())
    }

    fn print_grid(&self) {
        // Each row down is shifted half a cell right, mirroring the physical stagger.
        const CELL: usize = 4;
        for (row, &(first_col, keys_in_row)) in ROWS.iter().enumerate().rev() {
            let indent = " ".repeat((ROWS.len() - 1 - row) * CELL / 2 + first_col * CELL);
            let labels: String = keys_in_row
                .iter()
                .map(|(_, l)| format!("{l:<CELL$}"))
                .collect();
            let notes: String = keys_in_row
                .iter()
                .map(|&(k, _)| format!("{:<CELL$}", self.note(k).unwrap().name()))
                .collect();
            println!("{indent}{labels}\n{indent}{notes}\n");
        }
    }
}

/// Which palette colour each Keychron key shows: note keys by whether their
/// pitch is in the major scale on `tonic`, control keys in their own colour.
fn key_colors(layout: &Layout, tonic: u8) -> Colors {
    let mut colors: Colors = Default::default();
    let mut set = |key: KeyCode, color: u8| {
        let pos =
            keychron::matrix_pos(key).unwrap_or_else(|| panic!("no matrix position for {key:?}"));
        colors[pos.row as usize][pos.col as usize] = color;
    };
    for &(key, note) in &layout.keys {
        let degree = (note.0 + 12 - tonic) % 12;
        set(
            key,
            if MAJOR_SCALE.contains(&degree) {
                DIATONIC
            } else {
                CHROMATIC
            },
        );
    }
    for key in CONTROL_KEYS {
        set(key, CONTROL);
    }
    colors
}

/// Tracks which mapped keys are down and turns key transitions into MIDI.
///
/// Several keys can share a note (the layout repeats pitches). A note sounds
/// exactly while at least one key mapped to it is held: it starts when the
/// first such key goes down and stops when the last one comes up. This is
/// derived from `held` directly rather than kept as a separate counter.
///
/// Octave shifts apply to subsequent presses only: `held` records the note each
/// key actually started, and that is the note its release stops.
///
/// Sustain is left to the synth (MIDI CC 64): we send note-offs as usual and
/// the synth holds the sound while the pedal is down.
struct Player<'a> {
    layout: &'a Layout,
    out: Output,
    channel: u8, // 0-based
    /// Current octave shift; `layout` transposed by it always stays in MIDI range.
    octave: i32,
    held: HashMap<KeyCode, Note>,
    /// Sustain keys currently down; the pedal is down iff this is nonempty.
    sustain_held: HashSet<KeyCode>,
}

impl Player<'_> {
    fn is_sounding(&self, note: Note) -> bool {
        self.held.values().any(|&n| n == note)
    }

    fn press(&mut self, key: KeyCode, velocity: u8) -> Result<(), midir::SendError> {
        let Some(note) = self.layout.note(key) else {
            return Ok(());
        };
        if self.held.contains_key(&key) {
            return Ok(());
        }
        let note = note
            .transpose(12 * self.octave)
            .expect("octave shift keeps layout in range");
        let was_sounding = self.is_sounding(note);
        self.held.insert(key, note);
        if !was_sounding {
            self.out.send(&[0x90 | self.channel, note.0, velocity])?;
        }
        Ok(())
    }

    fn release(&mut self, key: KeyCode) -> Result<(), midir::SendError> {
        let Some(note) = self.held.remove(&key) else {
            return Ok(());
        };
        if !self.is_sounding(note) {
            self.out.send(&[0x80 | self.channel, note.0, 0])?;
        }
        Ok(())
    }

    /// Shifts subsequent notes by `delta` octaves, unless that would push any key
    /// out of MIDI range. Returns whether it shifted.
    fn shift_octave(&mut self, delta: i32) -> bool {
        let octave = self.octave + delta;
        let ok = self.layout.transposable(12 * octave);
        if ok {
            self.octave = octave;
        }
        ok
    }

    /// Tracks a sustain key going down or up, sending the pedal's CC 64 when
    /// the pedal as a whole changes state.
    fn sustain(&mut self, key: KeyCode, down: bool) -> Result<(), midir::SendError> {
        let was_down = !self.sustain_held.is_empty();
        if down {
            self.sustain_held.insert(key);
        } else {
            self.sustain_held.remove(&key);
        }
        let is_down = !self.sustain_held.is_empty();
        if is_down != was_down {
            self.out
                .send(&[0xB0 | self.channel, 64, if is_down { 127 } else { 0 }])?;
        }
        Ok(())
    }

    /// Silence everything, e.g. on exit or error, so no note is left hanging.
    fn release_all(&mut self) -> Result<(), midir::SendError> {
        // Pedal up first, or the synth would sustain through the note-offs.
        for key in self.sustain_held.clone() {
            self.sustain(key, false)?;
        }
        for key in self.held.keys().copied().collect::<Vec<_>>() {
            self.release(key)?;
        }
        assert!(self.held.is_empty());
        // All Notes Off, in case the synth disagrees with us about what's sounding.
        self.out.send(&[0xB0 | self.channel, 123, 0])
    }

    /// Polyphonic aftertouch for the note a held key is sounding.
    fn aftertouch(&mut self, key: KeyCode, pressure: u8) -> Result<(), midir::SendError> {
        match self.held.get(&key) {
            Some(note) => self.out.send(&[0xA0 | self.channel, note.0, pressure]),
            None => Ok(()),
        }
    }
}

/// The synth connection, optionally recording everything sent to it.
struct Output {
    connection: MidiOutputConnection,
    recorder: Option<recording::Recorder>,
}

impl Output {
    fn send(&mut self, message: &[u8]) -> Result<(), midir::SendError> {
        if let Some(recorder) = &mut self.recorder {
            recorder.record(message);
        }
        self.connection.send(message)
    }
}

fn main() {
    // Report errors with Display rather than the Debug formatting `main -> Result` uses.
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let layout = Layout::new(args.base)?;
    layout.print_grid();

    // Connect before taking over the keyboard: afterwards you can't type.
    let recording = args.record.as_deref().map(recording::create).transpose()?;
    let out = Output {
        connection: open_midi(&args)?,
        recorder: recording.as_ref().map(|_| recording::Recorder::new()),
    };

    let curve = || {
        Curve::new(
            Duration::from_secs_f64(args.fast_ms / 1000.0),
            Duration::from_secs_f64(args.slow_ms / 1000.0),
            args.min_velocity,
        )
    };
    let mut evdev_keys: Vec<KeyCode> = layout.keys.iter().map(|&(k, _)| k).collect();
    evdev_keys.extend(CONTROL_KEYS);
    let mut source: Box<dyn Source> = match args.input {
        InputKind::Laptop => {
            let path = args
                .device
                .clone()
                .unwrap_or_else(|| "/dev/input/by-path/platform-i8042-serio-0-event-kbd".into());
            Box::new(Evdev::open(
                &path,
                &evdev_keys,
                EvdevVelocity::Fixed(args.velocity),
            )?)
        }
        InputKind::KeychronStock => {
            let path = match args.device.clone() {
                Some(path) => path,
                None => keychron::device(keychron::KEYBOARD)?,
            };
            let note_keys = layout.keys.iter().map(|&(k, _)| k).collect();
            let velocity =
                EvdevVelocity::polled(curve()?, args.strike_depth, note_keys, args.show_timing)?;
            Box::new(Evdev::open(&path, &evdev_keys, velocity)?)
        }
        InputKind::Keychron => {
            let config = instrument::Config {
                start: args.start_depth,
                strike: args.strike_depth,
                release: args.release_depth,
                pressure_step: args.pressure_step,
            };
            if !(config.start < config.strike && config.release <= config.strike) {
                return Err(
                    "need --start-depth < --strike-depth and --release-depth <= --strike-depth"
                        .into(),
                );
            }
            let colors = key_colors(&layout, args.key);
            Box::new(Firmware::open(
                config,
                curve()?,
                PALETTE,
                &colors,
                args.show_timing,
            )?)
        }
    };
    println!("Play! Esc quits.");

    let mut player = Player {
        layout: &layout,
        out,
        channel: args.channel - 1,
        octave: 0,
        held: HashMap::new(),
        sustain_held: HashSet::new(),
    };
    let result = play(source.as_mut(), &mut player, args.strike_depth);
    let released = player.release_all();
    // Write the recording whatever happened, once every note has been ended.
    if let (Some(file), Some(recorder)) = (recording, &player.out.recorder) {
        recorder.write(file)?;
        println!("Recorded to {}.", args.record.as_ref().unwrap().display());
    }
    released?;
    result?;
    source.finish()
}

fn open_midi(args: &Args) -> Result<MidiOutputConnection, Box<dyn std::error::Error>> {
    let midi = MidiOutput::new("isomorphic")?;
    if let Some(name) = &args.virtual_port {
        let out = midi
            .create_virtual(name)
            .map_err(|e| format!("creating MIDI port: {e}"))?;
        println!("Created virtual MIDI port {name:?}; connect it to a synth yourself.");
        return Ok(out);
    }
    let mut all = Vec::new();
    let mut matches = Vec::new();
    for port in midi.ports() {
        let port_name = midi.port_name(&port)?;
        if port_name.contains(&args.synth) {
            matches.push((port, port_name.clone()));
        }
        all.push(port_name);
    }
    match <[_; 1]>::try_from(matches) {
        Ok([(port, port_name)]) => {
            let out = midi.connect(&port, "isomorphic").map_err(|e| format!("connecting to {port_name:?}: {e}"))?;
            println!("Connected to MIDI port {port_name:?}.");
            Ok(out)
        }
        Err(matches) => Err(format!(
            "need exactly one MIDI port containing {:?}, found {} (is the synth running?). Available ports:\n  {}",
            args.synth,
            matches.len(),
            all.join("\n  ")
        )
        .into()),
    }
}

/// Runs until the quit key is pressed or an error occurs.
fn play(
    source: &mut dyn Source,
    player: &mut Player,
    strike_depth: Travel,
) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        match source.next()? {
            Input::Down { key: QUIT_KEY, .. } => return Ok(()),
            Input::Down {
                key: key @ (OCTAVE_DOWN | OCTAVE_UP),
                ..
            } => {
                let delta = if key == OCTAVE_UP { 1 } else { -1 };
                if player.shift_octave(delta) {
                    println!("Octave {:+}", player.octave);
                } else {
                    println!("Octave {:+} is the limit", player.octave);
                }
            }
            Input::Down { key, .. } if SUSTAIN_KEYS.contains(&key) => player.sustain(key, true)?,
            Input::Down { key, velocity } => player.press(key, velocity)?,
            Input::Up { key } if SUSTAIN_KEYS.contains(&key) => player.sustain(key, false)?,
            Input::Up { key } => player.release(key)?,
            Input::Pressure { key, travel } => {
                player.aftertouch(key, aftertouch(travel, strike_depth))?
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
    fn control_keys_unmapped() {
        let l = Layout::new(48).unwrap();
        for key in [
            QUIT_KEY,
            OCTAVE_DOWN,
            OCTAVE_UP,
            SUSTAIN_KEYS[0],
            SUSTAIN_KEYS[1],
        ] {
            assert_eq!(l.note(key), None);
        }
        assert_eq!(note_of(&l, KeyCode::KEY_2), 48 + 2 + 5 * 3);
    }

    #[test]
    fn octave_limits() {
        let l = Layout::new(48).unwrap(); // spans 48..=85
        assert!(l.transposable(-48) && !l.transposable(-60));
        assert!(l.transposable(36) && !l.transposable(48));
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
    fn pitch_classes() {
        assert_eq!(parse_pitch_class("C"), Ok(0));
        assert_eq!(parse_pitch_class("f#"), Ok(6));
        assert_eq!(parse_pitch_class("Eb"), Ok(3));
        assert_eq!(parse_pitch_class("Cb"), Ok(11));
        assert_eq!(parse_pitch_class("B#"), Ok(0));
        assert!(parse_pitch_class("H").is_err());
        assert!(parse_pitch_class("C##").is_err());
        assert!(parse_pitch_class("").is_err());
    }

    #[test]
    fn aftertouch_range() {
        assert_eq!(aftertouch(0, 180), 0);
        assert_eq!(aftertouch(180, 180), 0);
        assert_eq!(aftertouch(210, 180), 64); // halfway, rounded
        assert_eq!(aftertouch(240, 180), 127);
        assert_eq!(aftertouch(245, 180), 127);
    }

    #[test]
    fn scale_colors() {
        let l = Layout::new(48).unwrap(); // z = C3
        let color = |colors: &Colors, key| {
            let p = keychron::matrix_pos(key).unwrap();
            colors[p.row as usize][p.col as usize]
        };
        let c_major = key_colors(&l, 0);
        assert_eq!(color(&c_major, KeyCode::KEY_Z), DIATONIC); // C
        assert_eq!(color(&c_major, KeyCode::KEY_V), CHROMATIC); // F#
        assert_eq!(color(&c_major, KeyCode::KEY_A), DIATONIC); // F
        assert_eq!(color(&c_major, KeyCode::KEY_Q), CHROMATIC); // Bb
        assert_eq!(color(&c_major, KeyCode::KEY_ESC), CONTROL);
        assert_eq!(color(&c_major, KeyCode::KEY_1), CONTROL);
        assert_eq!(c_major[2][0], 0); // Tab, unused: unlit
        let g_major = key_colors(&l, 7);
        assert_eq!(color(&g_major, KeyCode::KEY_V), DIATONIC); // F#
        assert_eq!(color(&g_major, KeyCode::KEY_A), CHROMATIC); // F
        // Exactly the note and control keys are lit.
        let lit = c_major.iter().flatten().filter(|&&c| c != 0).count();
        assert_eq!(lit, l.keys.len() + CONTROL_KEYS.len());
    }

    #[test]
    fn note_names() {
        assert_eq!(Note(60).name(), "C4");
        assert_eq!(Note(0).name(), "C-1");
        assert_eq!(Note(70).name(), "A#4");
    }
}
