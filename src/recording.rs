//! Records the MIDI messages sent to the synth as a Standard MIDI File.

use std::{
    fs::File,
    io::{self, BufWriter, Write},
    path::Path,
    time::{Duration, Instant},
};

/// Ticks per quarter note. At the tempo below (120 bpm, 500 000 µs per quarter
/// note, stated in the file) one tick is about 0.52 ms.
const TICKS_PER_QUARTER: u16 = 960;
const MICROS_PER_QUARTER: u32 = 500_000;

/// Messages as they were sent, with the time since the first one.
pub struct Recorder {
    start: Option<Instant>,
    messages: Vec<(Duration, Vec<u8>)>,
}

impl Recorder {
    pub fn new() -> Recorder {
        Recorder {
            start: None,
            messages: Vec::new(),
        }
    }

    /// Records `message` as sent now. The recording starts with the first message.
    pub fn record(&mut self, message: &[u8]) {
        let now = Instant::now();
        let start = *self.start.get_or_insert(now);
        self.messages.push((now - start, message.to_vec()));
    }

    /// Writes the recording as a format 0 Standard MIDI File.
    pub fn write(&self, file: File) -> io::Result<()> {
        let mut w = BufWriter::new(file);
        w.write_all(&smf(&self.messages))?;
        w.flush()
    }
}

/// Opens `path` for a recording, refusing to overwrite an existing file.
pub fn create(path: &Path) -> Result<File, String> {
    File::options()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| format!("creating {}: {e}", path.display()))
}

/// Nearest tick to `t`, computed exactly (no per-tick rounding to accumulate).
fn ticks(t: Duration) -> u64 {
    let num = t.as_nanos() * TICKS_PER_QUARTER as u128;
    let den = MICROS_PER_QUARTER as u128 * 1000;
    ((num + den / 2) / den) as u64
}

/// MIDI variable-length quantity: 7 bits per byte, most significant first,
/// high bit set on all but the last.
fn push_vlq(out: &mut Vec<u8>, mut value: u32) {
    let mut bytes = vec![(value & 0x7F) as u8];
    value >>= 7;
    while value > 0 {
        bytes.push((value & 0x7F) as u8 | 0x80);
        value >>= 7;
    }
    out.extend(bytes.iter().rev());
}

/// A whole format 0 file: one track with a tempo event, the messages (times in
/// non-decreasing order), and end of track.
fn smf(messages: &[(Duration, Vec<u8>)]) -> Vec<u8> {
    let mut track = Vec::new();
    push_vlq(&mut track, 0);
    track.extend([0xFF, 0x51, 0x03]); // set tempo
    track.extend(&MICROS_PER_QUARTER.to_be_bytes()[1..]);

    let mut last = 0;
    for (t, message) in messages {
        let tick = ticks(*t);
        assert!(tick >= last, "messages out of order");
        // A single delta longer than a VLQ allows would take over 8 hours.
        push_vlq(
            &mut track,
            u32::try_from(tick - last).expect("gap between messages too long"),
        );
        track.extend(message);
        last = tick;
    }
    push_vlq(&mut track, 0);
    track.extend([0xFF, 0x2F, 0x00]); // end of track

    let mut out = Vec::new();
    out.extend(b"MThd");
    out.extend(6u32.to_be_bytes());
    out.extend(0u16.to_be_bytes()); // format 0
    out.extend(1u16.to_be_bytes()); // one track
    out.extend(TICKS_PER_QUARTER.to_be_bytes());
    out.extend(b"MTrk");
    out.extend(
        u32::try_from(track.len())
            .expect("track too long")
            .to_be_bytes(),
    );
    out.extend(track);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vlq() {
        let enc = |v| {
            let mut out = Vec::new();
            push_vlq(&mut out, v);
            out
        };
        // Examples from the SMF specification.
        assert_eq!(enc(0), [0x00]);
        assert_eq!(enc(0x7F), [0x7F]);
        assert_eq!(enc(0x80), [0x81, 0x00]);
        assert_eq!(enc(0x2000), [0xC0, 0x00]);
        assert_eq!(enc(0x3FFF), [0xFF, 0x7F]);
        assert_eq!(enc(0x0FFF_FFFF), [0xFF, 0xFF, 0xFF, 0x7F]);
    }

    #[test]
    fn file_layout() {
        let messages = [
            (Duration::ZERO, vec![0x90, 60, 100]),
            (Duration::from_millis(500), vec![0x80, 60, 0]), // one beat: 960 ticks
        ];
        let f = smf(&messages);
        let track: &[u8] = &[
            0x00, 0xFF, 0x51, 0x03, 0x07, 0xA1, 0x20, // tempo 500000
            0x00, 0x90, 60, 100, //
            0x87, 0x40, 0x80, 60, 0, // delta 960
            0x00, 0xFF, 0x2F, 0x00,
        ];
        let mut expected = b"MThd\0\0\0\x06\0\0\0\x01\x03\xC0MTrk".to_vec();
        expected.extend((track.len() as u32).to_be_bytes());
        expected.extend(track);
        assert_eq!(f, expected);
    }

    #[test]
    fn tick_rounding() {
        assert_eq!(ticks(Duration::from_micros(520)), 1); // 0.52 ms ≈ 1 tick
        assert_eq!(ticks(Duration::from_micros(260)), 0);
        assert_eq!(ticks(Duration::from_secs(1)), 1920);
    }
}
