//! Reads per-key analog travel from a Keychron Q1 HE (ANSI) over raw HID.
//!
//! Protocol, from Keychron's qmk_firmware (branch 2025q3, commit 9ada9b7,
//! keyboards/keychron/common/analog_matrix/analog_matrix.c, AMC_GET_REALTIME_TRAVEL):
//! the host sends the 32-byte report `A9 30 row col`, and the keyboard answers
//! `A9 30 status row col travel/6 travel ...` with status 0 on success and
//! travel in 1/60 mm. One key per request; the firmware never sends these
//! unprompted, so the host has to poll.

use std::{
    error::Error,
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::Path,
    time::SystemTime,
};

use evdev::KeyCode;

/// The keyboard's ordinary key-event device.
pub const KEYBOARD: &str = "/dev/input/by-id/usb-Keychron_Keychron_Q1_HE-event-kbd";
/// The raw HID (VIA, usage page 0xFF60) interface.
pub const HIDRAW: &str = "/dev/input/by-id/usb-Keychron_Keychron_Q1_HE-if01-hidraw";

const REPORT_LEN: usize = 32;
const CMD_ANALOG_MATRIX: u8 = 0xA9;
const GET_REALTIME_TRAVEL: u8 = 0x30;

/// Key travel in 1/60 mm: 0 at rest, 240 = 4.0 mm = full travel; the firmware
/// clamps at 245.
pub type Travel = u8;

/// A key's position in the keyboard's 6x15 switch matrix.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MatrixPos {
    pub row: u8,
    pub col: u8,
}

/// Matrix position of a key on the ANSI Q1 HE, for the keys we play
/// (from `LAYOUT_ansi_82` in keyboards/keychron/q1_he/ansi_encoder/keymaps/default/keymap.c).
pub fn matrix_pos(key: KeyCode) -> Option<MatrixPos> {
    use KeyCode as K;
    let (row, col) = match key {
        K::KEY_GRAVE => (1, 0),
        K::KEY_1 => (1, 1),
        K::KEY_2 => (1, 2),
        K::KEY_3 => (1, 3),
        K::KEY_4 => (1, 4),
        K::KEY_5 => (1, 5),
        K::KEY_6 => (1, 6),
        K::KEY_7 => (1, 7),
        K::KEY_8 => (1, 8),
        K::KEY_9 => (1, 9),
        K::KEY_0 => (1, 10),
        K::KEY_MINUS => (1, 11),
        K::KEY_EQUAL => (1, 12),
        K::KEY_Q => (2, 1),
        K::KEY_W => (2, 2),
        K::KEY_E => (2, 3),
        K::KEY_R => (2, 4),
        K::KEY_T => (2, 5),
        K::KEY_Y => (2, 6),
        K::KEY_U => (2, 7),
        K::KEY_I => (2, 8),
        K::KEY_O => (2, 9),
        K::KEY_P => (2, 10),
        K::KEY_LEFTBRACE => (2, 11),
        K::KEY_RIGHTBRACE => (2, 12),
        K::KEY_A => (3, 1),
        K::KEY_S => (3, 2),
        K::KEY_D => (3, 3),
        K::KEY_F => (3, 4),
        K::KEY_G => (3, 5),
        K::KEY_H => (3, 6),
        K::KEY_J => (3, 7),
        K::KEY_K => (3, 8),
        K::KEY_L => (3, 9),
        K::KEY_SEMICOLON => (3, 10),
        K::KEY_APOSTROPHE => (3, 11),
        // Row 4 has no switch at column 1 (the wide left shift is at 0) or 11.
        K::KEY_Z => (4, 2),
        K::KEY_X => (4, 3),
        K::KEY_C => (4, 4),
        K::KEY_V => (4, 5),
        K::KEY_B => (4, 6),
        K::KEY_N => (4, 7),
        K::KEY_M => (4, 8),
        K::KEY_COMMA => (4, 9),
        K::KEY_DOT => (4, 10),
        K::KEY_SLASH => (4, 12),
        _ => return None,
    };
    Some(MatrixPos { row, col })
}

/// One travel reading, timestamped at the midpoint of the request/reply round
/// trip (the firmware reads the value somewhere in between).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Sample {
    pub time: SystemTime,
    pub travel: Travel,
}

pub struct Hid(File);

impl Hid {
    pub fn open(path: impl AsRef<Path>) -> std::io::Result<Hid> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map(Hid)
    }

    pub fn travel(&mut self, pos: MatrixPos) -> Result<Sample, Box<dyn Error>> {
        // hidraw writes start with the report ID, 0 since this interface has none.
        let mut request = [0u8; 1 + REPORT_LEN];
        request[1..5].copy_from_slice(&[CMD_ANALOG_MATRIX, GET_REALTIME_TRAVEL, pos.row, pos.col]);
        let sent = SystemTime::now();
        self.0.write_all(&request)?;
        let reply = loop {
            let mut buf = [0u8; REPORT_LEN];
            let n = self.0.read(&mut buf)?;
            if n != REPORT_LEN {
                return Err(format!("short raw HID read: {n} bytes").into());
            }
            // Skip unsolicited reports, e.g. layer-change notifications (0xA3).
            if buf[..2] == [CMD_ANALOG_MATRIX, GET_REALTIME_TRAVEL]
                && buf[3..5] == [pos.row, pos.col]
            {
                break buf;
            }
        };
        let received = SystemTime::now();
        if reply[2] != 0 {
            return Err(format!(
                "keyboard rejected travel request for {pos:?} (status {})",
                reply[2]
            )
            .into());
        }
        let half_trip = received.duration_since(sent).unwrap_or_default() / 2;
        Ok(Sample {
            time: sent + half_trip,
            travel: reply[6],
        })
    }
}
