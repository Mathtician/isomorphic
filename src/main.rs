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

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::mpsc,
    thread,
    time::{Duration, SystemTime},
};

use clap::Parser;
use evdev::{Device, EventSummary, KeyCode};
use isomorphic::{
    keychron::{self, Hid},
    velocity::{Curve, Measurer},
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

// evdev key event values.
const RELEASE: i32 = 0;
const PRESS: i32 = 1;
const AUTOREPEAT: i32 = 2;

#[derive(Parser)]
#[command(
    about = "Play the keyboard as a Wicki-Hayden MIDI controller. ` and 1 shift octaves; Esc quits."
)]
struct Args {
    /// evdev keyboard device to read (and grab) [default: the laptop keyboard,
    /// or the Keychron's with --keychron]
    #[arg(long)]
    device: Option<PathBuf>,
    /// Play a Keychron Q1 HE with velocity measured from its analog key travel
    #[arg(long)]
    keychron: bool,
    /// With --keychron: depth at which a key sounds, in 1/60 mm (240 = full
    /// travel). Must be deeper than the actuation point set in Keychron Launcher.
    #[arg(long, default_value_t = 180, value_parser = clap::value_parser!(u8).range(1..=230))]
    strike_depth: u8,
    /// With --keychron: descent time (actuation to strike depth, ms) at or
    /// below which velocity is 127
    #[arg(long, default_value_t = 2.0)]
    fast_ms: f64,
    /// With --keychron: descent time (ms) at or above which velocity is --min-velocity
    #[arg(long, default_value_t = 40.0)]
    slow_ms: f64,
    /// With --keychron: velocity of the slowest presses
    #[arg(long, default_value_t = 32, value_parser = clap::value_parser!(u8).range(1..=127))]
    min_velocity: u8,
    /// With --keychron: print each note's descent time and velocity, for tuning
    /// --fast-ms and --slow-ms
    #[arg(long)]
    show_timing: bool,
    /// MIDI note number of the Z key (48 = C3)
    #[arg(long, default_value_t = 48)]
    base: u8,
    /// MIDI channel
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u8).range(1..=16))]
    channel: u8,
    /// Note-on velocity (laptop keys aren't velocity-sensitive)
    #[arg(long, conflicts_with = "keychron", default_value_t = 100, value_parser = clap::value_parser!(u8).range(1..=127))]
    velocity: u8,
    /// Connect directly to the one MIDI input port whose name contains this
    /// (e.g. "FLUID" for fluidsynth). The connection is made before the
    /// keyboard is grabbed, since afterwards you can't type anywhere else.
    #[arg(long, default_value = "FLUID", conflicts_with = "virtual_port")]
    synth: String,
    /// Instead of connecting to a synth, create a virtual MIDI port with this
    /// name and leave the wiring to you (e.g. with the mouse in qpwgraph).
    #[arg(long)]
    virtual_port: Option<String>,
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

/// Tracks which mapped keys are down and turns key transitions into MIDI.
///
/// Several keys can share a note (the layout repeats pitches). A note sounds
/// exactly while at least one key mapped to it is held: it starts when the
/// first such key goes down and stops when the last one comes up. This is
/// derived from `held` directly rather than kept as a separate counter.
///
/// Octave shifts apply to subsequent presses only: `held` records the note each
/// key actually started, and that is the note its release stops.
struct Player<'a> {
    layout: &'a Layout,
    out: MidiOutputConnection,
    channel: u8, // 0-based
    /// Current octave shift; `layout` transposed by it always stays in MIDI range.
    octave: i32,
    held: HashMap<KeyCode, Note>,
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

    /// Silence everything, e.g. on exit or error, so no note is left hanging.
    fn release_all(&mut self) -> Result<(), midir::SendError> {
        for key in self.held.keys().copied().collect::<Vec<_>>() {
            self.release(key)?;
        }
        assert!(self.held.is_empty());
        // All Notes Off, in case the synth disagrees with us about what's sounding.
        self.out.send(&[0xB0 | self.channel, 123, 0])
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

    let device_path = args.device.clone().unwrap_or_else(|| {
        PathBuf::from(if args.keychron {
            keychron::KEYBOARD
        } else {
            "/dev/input/by-path/platform-i8042-serio-0-event-kbd"
        })
    });
    let mut device = Device::open(&device_path)
        .map_err(|e| format!("opening {}: {e} (no read access?)", device_path.display()))?;
    let supported = device.supported_keys().ok_or("device reports no keys")?;
    for &(key, _) in layout.keys.iter().chain(&[
        (QUIT_KEY, Note(0)),
        (OCTAVE_DOWN, Note(0)),
        (OCTAVE_UP, Note(0)),
    ]) {
        if !supported.contains(key) {
            return Err(format!("device {:?} lacks key {key:?}", device.name()).into());
        }
    }

    let mut velocity = if args.keychron {
        for &(key, _) in &layout.keys {
            assert!(
                keychron::matrix_pos(key).is_some(),
                "no matrix position for {key:?}"
            );
        }
        let hid = Hid::open(keychron::HIDRAW)
            .map_err(|e| format!("opening {}: {e} (no read/write access?)", keychron::HIDRAW))?;
        let curve = Curve::new(
            Duration::from_secs_f64(args.fast_ms / 1000.0),
            Duration::from_secs_f64(args.slow_ms / 1000.0),
            args.min_velocity,
        )?;
        Velocity::Measured(Measurer::new(hid, curve, args.strike_depth))
    } else {
        Velocity::Fixed(args.velocity)
    };

    let out = open_midi(&args)?;
    println!("Reading {:?}.", device.name().unwrap_or("?"));

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

    // Key events come from a thread blocking on the device, so the main loop
    // can poll key travel while waiting for them. The grab ends when the
    // process exits and the device is closed.
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || read_keys(device, tx));

    let mut player = Player {
        layout: &layout,
        out,
        channel: args.channel - 1,
        octave: 0,
        held: HashMap::new(),
    };
    let result = play(&rx, &mut player, &mut velocity, args.show_timing);
    player.release_all()?;
    result
}

/// Where note-on velocities come from.
enum Velocity {
    Fixed(u8),
    Measured(Measurer),
}

struct KeyEvent {
    key: KeyCode,
    value: i32,
    time: SystemTime,
}

/// Forwards key events until the device fails or the receiver is gone.
fn read_keys(mut device: Device, tx: mpsc::Sender<Result<KeyEvent, String>>) {
    loop {
        let events = match device.fetch_events() {
            Ok(events) => events,
            Err(e) => {
                let _ = tx.send(Err(format!("reading keyboard: {e}")));
                return;
            }
        };
        for event in events {
            if let EventSummary::Key(_, key, value) = event.destructure() {
                let time = event.timestamp();
                if tx.send(Ok(KeyEvent { key, value, time })).is_err() {
                    return;
                }
            }
        }
    }
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
///
/// While any key is on its way down (only with measured velocity), key events
/// are checked between travel polls; otherwise we just wait for the next one.
fn play(
    rx: &mpsc::Receiver<Result<KeyEvent, String>>,
    player: &mut Player,
    velocity: &mut Velocity,
    show_timing: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        let polling = match velocity {
            Velocity::Measured(m) => !m.is_idle(),
            Velocity::Fixed(_) => false,
        };
        let event = if polling {
            match rx.try_recv() {
                Ok(event) => Some(event),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => {
                    unreachable!("reader exits only after sending an error, which ends play")
                }
            }
        } else {
            Some(
                rx.recv()
                    .expect("reader thread reports errors before exiting"),
            )
        };

        let Some(event) = event else {
            let Velocity::Measured(m) = velocity else {
                unreachable!("only measured velocity polls")
            };
            if let Some(strike) = m.poll()? {
                if show_timing {
                    println!(
                        "{:?}: {:.1} ms -> velocity {}",
                        strike.key,
                        strike.descent.as_secs_f64() * 1000.0,
                        strike.velocity
                    );
                }
                player.press(strike.key, strike.velocity)?;
            }
            continue;
        };

        let KeyEvent { key, value, time } = event?;
        match value {
            PRESS if key == QUIT_KEY => return Ok(()),
            PRESS if key == OCTAVE_DOWN || key == OCTAVE_UP => {
                let delta = if key == OCTAVE_UP { 1 } else { -1 };
                if player.shift_octave(delta) {
                    println!("Octave {:+}", player.octave);
                } else {
                    println!("Octave {:+} is the limit", player.octave);
                }
            }
            PRESS if player.layout.note(key).is_some() => match velocity {
                Velocity::Fixed(v) => player.press(key, *v)?,
                Velocity::Measured(m) => {
                    let pos = keychron::matrix_pos(key).expect("checked at startup");
                    m.press(key, pos, time);
                }
            },
            PRESS => {}
            RELEASE => {
                if let Velocity::Measured(m) = velocity {
                    m.release(key);
                }
                player.release(key)?;
            }
            AUTOREPEAT => {}
            v => return Err(format!("unexpected key event value {v} for {key:?}").into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

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
        for key in [QUIT_KEY, OCTAVE_DOWN, OCTAVE_UP] {
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
    fn note_names() {
        assert_eq!(Note(60).name(), "C4");
        assert_eq!(Note(0).name(), "C-1");
        assert_eq!(Note(70).name(), "A#4");
    }
}
