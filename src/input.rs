//! Where key presses come from: the three input modes, behind one interface.

use std::{
    collections::{HashSet, VecDeque},
    error::Error,
    path::Path,
    sync::mpsc,
    thread,
    time::{Duration, Instant, SystemTime},
};

use evdev::{Device, EventSummary, KeyCode};
use isomorphic::{
    instrument::{self, Batch, Colors, Config, Event, EventBatch, Instrument},
    keychron::{self, Hid, Travel},
    velocity::{Curve, Measurer},
};

/// What the player needs to know about the keyboard.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Input {
    Down {
        key: KeyCode,
        velocity: u8,
    },
    Up {
        key: KeyCode,
    },
    /// Current travel of a held key (instrument firmware only).
    Pressure {
        key: KeyCode,
        travel: Travel,
    },
}

pub trait Source {
    /// Blocks until the next input.
    fn next(&mut self) -> Result<Input, Box<dyn Error>>;
    /// Called once when playing stops normally.
    fn finish(&mut self) -> Result<(), Box<dyn Error>> {
        Ok(())
    }
}

// evdev key event values.
const RELEASE: i32 = 0;
const PRESS: i32 = 1;
const AUTOREPEAT: i32 = 2;

struct KeyEvent {
    key: KeyCode,
    value: i32,
    time: SystemTime,
}

/// Key events from an evdev keyboard, grabbed so they don't also type. Note
/// velocities are fixed, or measured by polling a stock-firmware Keychron.
pub struct Evdev {
    rx: mpsc::Receiver<Result<KeyEvent, String>>,
    velocity: EvdevVelocity,
    pending: VecDeque<Input>,
}

pub enum EvdevVelocity {
    Fixed(u8),
    Polled {
        measurer: Measurer,
        note_keys: HashSet<KeyCode>,
        show_timing: bool,
    },
}

impl EvdevVelocity {
    pub fn polled(
        curve: Curve,
        strike_depth: Travel,
        note_keys: HashSet<KeyCode>,
        show_timing: bool,
    ) -> Result<Self, Box<dyn Error>> {
        for &key in &note_keys {
            assert!(
                keychron::matrix_pos(key).is_some(),
                "no matrix position for {key:?}"
            );
        }
        let hid = Hid::open(keychron::HIDRAW)
            .map_err(|e| format!("opening {}: {e} (no read/write access?)", keychron::HIDRAW))?;
        Ok(EvdevVelocity::Polled {
            measurer: Measurer::new(hid, curve, strike_depth),
            note_keys,
            show_timing,
        })
    }
}

impl Evdev {
    /// Opens and grabs `path`, after checking it has every key in `keys`.
    pub fn open(
        path: &Path,
        keys: &[KeyCode],
        velocity: EvdevVelocity,
    ) -> Result<Evdev, Box<dyn Error>> {
        let mut device = Device::open(path)
            .map_err(|e| format!("opening {}: {e} (no read access?)", path.display()))?;
        let supported = device.supported_keys().ok_or("device reports no keys")?;
        if let Some(key) = keys.iter().find(|&&k| !supported.contains(k)) {
            return Err(format!("device {:?} lacks key {key:?}", device.name()).into());
        }
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

        // Key events come from a thread blocking on the device, so polling can
        // go on while waiting for them. The grab ends when the process exits.
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || read_keys(device, tx));
        Ok(Evdev {
            rx,
            velocity,
            pending: VecDeque::new(),
        })
    }
}

impl Source for Evdev {
    fn next(&mut self) -> Result<Input, Box<dyn Error>> {
        loop {
            if let Some(input) = self.pending.pop_front() {
                return Ok(input);
            }
            let polling = match &self.velocity {
                EvdevVelocity::Polled { measurer, .. } => !measurer.is_idle(),
                EvdevVelocity::Fixed(_) => false,
            };
            let event = if polling {
                match self.rx.try_recv() {
                    Ok(event) => Some(event),
                    Err(mpsc::TryRecvError::Empty) => None,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        unreachable!("the reader exits only after sending an error")
                    }
                }
            } else {
                Some(
                    self.rx
                        .recv()
                        .expect("the reader exits only after sending an error"),
                )
            };

            let Some(event) = event else {
                let EvdevVelocity::Polled {
                    measurer,
                    show_timing,
                    ..
                } = &mut self.velocity
                else {
                    unreachable!("only polled velocity polls")
                };
                if let Some(strike) = measurer.poll()? {
                    if *show_timing {
                        println!(
                            "{:?}: {:.1} ms -> velocity {}",
                            strike.key,
                            ms(strike.descent),
                            strike.velocity
                        );
                    }
                    self.pending.push_back(Input::Down {
                        key: strike.key,
                        velocity: strike.velocity,
                    });
                }
                continue;
            };

            let KeyEvent { key, value, time } = event?;
            match (value, &mut self.velocity) {
                (
                    PRESS,
                    EvdevVelocity::Polled {
                        measurer,
                        note_keys,
                        ..
                    },
                ) if note_keys.contains(&key) => measurer.press(
                    key,
                    keychron::matrix_pos(key).expect("checked in polled()"),
                    time,
                ),
                // Control keys act immediately; their velocity is unused.
                (PRESS, EvdevVelocity::Polled { .. }) => {
                    self.pending.push_back(Input::Down { key, velocity: 0 })
                }
                (PRESS, EvdevVelocity::Fixed(velocity)) => self.pending.push_back(Input::Down {
                    key,
                    velocity: *velocity,
                }),
                (RELEASE, velocity) => {
                    if let EvdevVelocity::Polled { measurer, .. } = velocity {
                        measurer.release(key);
                    }
                    self.pending.push_back(Input::Up { key });
                }
                (AUTOREPEAT, _) => {}
                (v, _) => return Err(format!("unexpected key event value {v} for {key:?}").into()),
            }
        }
    }
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

/// A Keychron running the instrument firmware, which measures strikes itself.
pub struct Firmware {
    instrument: Instrument,
    rx: mpsc::Receiver<EventBatch>,
    curve: Curve,
    show_timing: bool,
    next_ping: Instant,
    pending: VecDeque<Input>,
}

impl Firmware {
    /// Sets the key colours and switches the keyboard into instrument mode, at
    /// which point it stops typing.
    pub fn open(
        config: Config,
        curve: Curve,
        palette: [[u8; 3]; 3],
        colors: &Colors,
        show_timing: bool,
    ) -> Result<Firmware, Box<dyn Error>> {
        let (mut instrument, rx) = Instrument::open(keychron::HIDRAW)?;
        instrument.set_colors(palette, colors)?;
        instrument.stats()?; // reset, so the stats printed at the end cover playing
        instrument.enter(config)?;
        println!("Keychron in instrument mode.");
        Ok(Firmware {
            instrument,
            rx,
            curve,
            show_timing,
            next_ping: Instant::now() + instrument::PING_INTERVAL,
            pending: VecDeque::new(),
        })
    }

    fn convert(&mut self, Batch { events, overflowed }: Batch) -> Result<(), Box<dyn Error>> {
        if overflowed {
            return Err("the keyboard's event queue overflowed, so notes were lost".into());
        }
        for event in events {
            // Keys outside the ones we use (Tab, arrows, ...) do nothing.
            let input = match event {
                Event::NoteOn { pos, descent } => keychron::key_at(pos).map(|key| {
                    let velocity = self.curve.velocity(descent);
                    if self.show_timing {
                        println!("{key:?}: {:.2} ms -> velocity {velocity}", ms(descent));
                    }
                    Input::Down { key, velocity }
                }),
                Event::NoteOff { pos } => keychron::key_at(pos).map(|key| Input::Up { key }),
                Event::Pressure { pos, travel } => {
                    keychron::key_at(pos).map(|key| Input::Pressure { key, travel })
                }
            };
            self.pending.extend(input);
        }
        Ok(())
    }
}

impl Source for Firmware {
    fn next(&mut self) -> Result<Input, Box<dyn Error>> {
        loop {
            // Checked every time round, since events can keep the wait below
            // from ever timing out.
            let now = Instant::now();
            if now >= self.next_ping {
                self.instrument.ping()?;
                self.next_ping = now + instrument::PING_INTERVAL;
            }
            if let Some(input) = self.pending.pop_front() {
                return Ok(input);
            }
            match self
                .rx
                .recv_timeout(self.next_ping.saturating_duration_since(now))
            {
                Ok(batch) => self.convert(batch?)?,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    unreachable!("the reader exits only after sending an error")
                }
            }
        }
    }

    fn finish(&mut self) -> Result<(), Box<dyn Error>> {
        let stats = self.instrument.stats()?;
        self.instrument.exit()?;
        println!(
            "Keychron back to normal. It scanned {} times: every {:?} on average, {:?} at worst.",
            stats.scans, stats.mean_period, stats.max_period
        );
        Ok(())
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}
