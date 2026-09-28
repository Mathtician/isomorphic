//! Host side of the instrument-mode firmware (firmware/keymap/instrument.c,
//! which documents the protocol): commands over the Keychron's raw HID
//! interface, and the key events it sends back while instrument mode is on.

use std::{
    error::Error,
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::Path,
    sync::mpsc,
    thread,
    time::Duration,
};

use crate::keychron::{COLS, MatrixPos, ROWS, Travel};

const REPORT_LEN: usize = 32;
const ID_CUSTOM_SET_VALUE: u8 = 0x07;
const ID_UNHANDLED: u8 = 0xFF;
const CHANNEL: u8 = 0x49;
const EVENT_REPORT: u8 = 0x49;
const PROTOCOL_VERSION: u8 = 1;
const REPLY_TIMEOUT: Duration = Duration::from_millis(500);
/// The firmware leaves instrument mode after 1 s without a command.
pub const PING_INTERVAL: Duration = Duration::from_millis(250);
/// Unit of the descent times in note-on events.
const DESCENT_UNIT: Duration = Duration::from_micros(10);

#[derive(Clone, Copy, Debug)]
#[repr(u8)]
enum Command {
    Version = 0,
    Enter = 1,
    Ping = 2,
    Exit = 3,
    SetPalette = 4,
    SetRowColors = 5,
    GetStats = 6,
}

/// Depths in 1/60 mm; see ic_config_t in firmware/keymap/instrument_core.h.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// Descent timing starts here.
    pub start: Travel,
    /// The note sounds here.
    pub strike: Travel,
    /// A sounding note stops when the key rises above this.
    pub release: Travel,
    /// Smallest travel change reported as new pressure.
    pub pressure_step: Travel,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Event {
    /// The key reached strike depth, `descent` after passing the start depth.
    NoteOn {
        pos: MatrixPos,
        descent: Duration,
    },
    NoteOff {
        pos: MatrixPos,
    },
    /// A struck key's current travel.
    Pressure {
        pos: MatrixPos,
        travel: Travel,
    },
}

/// One palette colour per key; 0 is unlit, 1..=3 index the palette.
pub type Colors = [[u8; COLS as usize]; ROWS as usize];

#[derive(Debug)]
pub struct Stats {
    pub scans: u32,
    pub max_period: Duration,
    pub mean_period: Duration,
}

/// A batch of events from one report, or the reader's failure.
pub type EventBatch = Result<Batch, String>;

#[derive(Debug)]
pub struct Batch {
    pub events: Vec<Event>,
    /// The firmware's event queue overflowed, so some note events were lost.
    pub overflowed: bool,
}

pub struct Instrument {
    file: File,
    replies: mpsc::Receiver<[u8; REPORT_LEN]>,
    active: bool,
}

impl Instrument {
    /// Opens the raw HID device and checks the firmware speaks our protocol.
    /// Event batches arrive on the returned receiver once `enter` succeeds.
    pub fn open(
        path: impl AsRef<Path>,
    ) -> Result<(Instrument, mpsc::Receiver<EventBatch>), Box<dyn Error>> {
        let path = path.as_ref();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| format!("opening {}: {e} (no read/write access?)", path.display()))?;
        let (reply_tx, replies) = mpsc::channel();
        let (event_tx, events) = mpsc::channel();
        let reader = file.try_clone()?;
        thread::spawn(move || read_reports(reader, reply_tx, event_tx));

        let mut instrument = Instrument {
            file,
            replies,
            active: false,
        };
        let reply = instrument.command(Command::Version, &[])?;
        if reply[0] != PROTOCOL_VERSION || reply[1] != ROWS || reply[2] != COLS {
            return Err(format!(
                "firmware speaks protocol {} with a {}x{} matrix; expected {PROTOCOL_VERSION} with {ROWS}x{COLS}",
                reply[0], reply[1], reply[2]
            )
            .into());
        }
        Ok((instrument, events))
    }

    fn command(
        &mut self,
        command: Command,
        payload: &[u8],
    ) -> Result<[u8; REPORT_LEN - 4], Box<dyn Error>> {
        // hidraw writes start with the report ID, 0 since this interface has none.
        let mut request = [0u8; 1 + REPORT_LEN];
        request[1..4].copy_from_slice(&[ID_CUSTOM_SET_VALUE, CHANNEL, command as u8]);
        request[5..5 + payload.len()].copy_from_slice(payload);
        self.file.write_all(&request)?;
        loop {
            let reply = self
                .replies
                .recv_timeout(REPLY_TIMEOUT)
                .map_err(|_| format!("no reply from the keyboard to {command:?}"))?;
            if reply[0] == ID_UNHANDLED {
                return Err("the keyboard doesn't know instrument mode; is the isomorphic firmware flashed?".into());
            }
            if reply[2] != command as u8 {
                continue; // a late reply to an earlier command
            }
            return match reply[3] {
                0 => Ok(reply[4..].try_into().unwrap()),
                1 => Err(format!("firmware: unknown command {command:?}").into()),
                2 => Err(format!("firmware: bad argument to {command:?}: {payload:?}").into()),
                3 => Err("firmware left instrument mode (did the host stall for a second?)".into()),
                s => Err(format!("firmware: status {s} for {command:?}").into()),
            };
        }
    }

    /// Sets the three palette colours and which one each key shows. Takes
    /// effect while instrument mode is on.
    pub fn set_colors(
        &mut self,
        palette: [[u8; 3]; 3],
        colors: &Colors,
    ) -> Result<(), Box<dyn Error>> {
        for (i, rgb) in palette.iter().enumerate() {
            self.command(Command::SetPalette, &[i as u8 + 1, rgb[0], rgb[1], rgb[2]])?;
        }
        for (row, indices) in colors.iter().enumerate() {
            let mut payload = [0u8; 1 + COLS as usize];
            payload[0] = row as u8;
            payload[1..].copy_from_slice(indices);
            self.command(Command::SetRowColors, &payload)?;
        }
        Ok(())
    }

    /// Turns instrument mode on: keys stop typing and events start flowing.
    /// Call `ping` at least every PING_INTERVAL from then on.
    pub fn enter(&mut self, config: Config) -> Result<(), Box<dyn Error>> {
        self.command(
            Command::Enter,
            &[
                config.start,
                config.strike,
                config.release,
                config.pressure_step,
            ],
        )?;
        self.active = true;
        Ok(())
    }

    pub fn ping(&mut self) -> Result<(), Box<dyn Error>> {
        self.command(Command::Ping, &[]).map(|_| ())
    }

    /// Scan statistics since the previous call (or since power-on).
    pub fn stats(&mut self) -> Result<Stats, Box<dyn Error>> {
        let p = self.command(Command::GetStats, &[])?;
        Ok(Stats {
            scans: u32::from_le_bytes(p[0..4].try_into().unwrap()),
            max_period: Duration::from_micros(u16::from_le_bytes([p[4], p[5]]).into()),
            mean_period: Duration::from_micros(u16::from_le_bytes([p[6], p[7]]).into()),
        })
    }

    pub fn exit(&mut self) -> Result<(), Box<dyn Error>> {
        self.active = false;
        self.command(Command::Exit, &[]).map(|_| ())
    }
}

impl Drop for Instrument {
    fn drop(&mut self) {
        // Best effort: if this fails the firmware's watchdog gets there within a second.
        if self.active {
            let _ = self.exit();
        }
    }
}

/// Sorts incoming reports into command replies and event batches until the
/// device fails or both receivers are gone.
fn read_reports(
    mut file: File,
    replies: mpsc::Sender<[u8; REPORT_LEN]>,
    events: mpsc::Sender<EventBatch>,
) {
    loop {
        let mut report = [0u8; REPORT_LEN];
        match file.read(&mut report) {
            Ok(REPORT_LEN) => {}
            Ok(n) => {
                let _ = events.send(Err(format!("short raw HID read: {n} bytes")));
                return;
            }
            Err(e) => {
                let _ = events.send(Err(format!("reading raw HID: {e}")));
                return;
            }
        }
        let delivered = match report[0] {
            EVENT_REPORT => events.send(parse_events(&report)).is_ok(),
            ID_CUSTOM_SET_VALUE if report[1] == CHANNEL => replies.send(report).is_ok(),
            ID_UNHANDLED => replies.send(report).is_ok(),
            _ => true, // other raw HID traffic, e.g. Keychron's layer notifications
        };
        if !delivered {
            return;
        }
    }
}

fn parse_events(report: &[u8; REPORT_LEN]) -> EventBatch {
    let count = report[2] as usize;
    if 3 + 5 * count > REPORT_LEN {
        return Err(format!("event report claims {count} events"));
    }
    let mut events = Vec::with_capacity(count);
    for e in report[3..3 + 5 * count].chunks_exact(5) {
        let pos = MatrixPos {
            row: e[1],
            col: e[2],
        };
        let value = u16::from_le_bytes([e[3], e[4]]);
        events.push(match e[0] {
            1 => Event::NoteOn {
                pos,
                descent: DESCENT_UNIT * value.into(),
            },
            2 => Event::NoteOff { pos },
            3 => Event::Pressure {
                pos,
                travel: u8::try_from(value)
                    .map_err(|_| format!("pressure travel {value} out of range"))?,
            },
            t => return Err(format!("unknown event type {t}")),
        });
    }
    Ok(Batch {
        events,
        overflowed: report[1] & 1 != 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_event_report() {
        let mut r = [0u8; REPORT_LEN];
        r[..3].copy_from_slice(&[EVENT_REPORT, 1, 3]);
        r[3..8].copy_from_slice(&[1, 4, 2, 0x10, 0x27]); // note-on, 10000 units
        r[8..13].copy_from_slice(&[3, 4, 2, 200, 0]);
        r[13..18].copy_from_slice(&[2, 4, 2, 0, 0]);
        let batch = parse_events(&r).unwrap();
        let pos = MatrixPos { row: 4, col: 2 };
        assert!(batch.overflowed);
        assert_eq!(
            batch.events,
            [
                Event::NoteOn {
                    pos,
                    descent: Duration::from_millis(100)
                },
                Event::Pressure { pos, travel: 200 },
                Event::NoteOff { pos },
            ]
        );
    }

    #[test]
    fn rejects_malformed_reports() {
        let mut r = [0u8; REPORT_LEN];
        r[..3].copy_from_slice(&[EVENT_REPORT, 0, 6]); // 6 events don't fit
        assert!(parse_events(&r).is_err());
        r[2] = 1;
        r[3] = 9; // unknown type
        assert!(parse_events(&r).is_err());
    }
}
