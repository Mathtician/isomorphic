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
    path::{Path, PathBuf},
    time::SystemTime,
};

use evdev::KeyCode;

/// Suffixes of the keyboard's /dev/input/by-id names: its ordinary key-event
/// device, and its raw HID (VIA, usage page 0xFF60) interface.
pub const KEYBOARD: &str = "-event-kbd";
pub const HIDRAW: &str = "-if01-hidraw";

/// Finds the keyboard's device whose by-id name ends in `suffix`. The name
/// starts with the USB manufacturer and product, and includes a serial number
/// only on some firmware (ours has one, stock doesn't), so match around it.
pub fn device(suffix: &str) -> Result<PathBuf, String> {
    const DIR: &str = "/dev/input/by-id";
    const PREFIX: &str = "usb-Keychron_Keychron_Q1_HE";
    let entries = std::fs::read_dir(DIR).map_err(|e| format!("listing {DIR}: {e}"))?;
    let mut found: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name.starts_with(PREFIX) && name.ends_with(suffix)
        })
        .map(|e| e.path())
        .collect();
    match found.len() {
        1 => Ok(found.pop().unwrap()),
        0 => Err(format!(
            "no {DIR}/{PREFIX}*{suffix}; is the Keychron plugged in (by USB)?"
        )),
        _ => Err(format!("several Keychron devices match: {found:?}")),
    }
}

/// Matrix dimensions.
pub const ROWS: u8 = 6;
pub const COLS: u8 = 15;

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

/// Matrix positions on the ANSI Q1 HE of the keys we use (from `LAYOUT_ansi_82`
/// in keyboards/keychron/q1_he/ansi_encoder/keymaps/default/keymap.c).
const KEYS: &[(KeyCode, MatrixPos)] = &[
    (KeyCode::KEY_ESC, MatrixPos { row: 0, col: 0 }),
    (KeyCode::KEY_GRAVE, MatrixPos { row: 1, col: 0 }),
    (KeyCode::KEY_1, MatrixPos { row: 1, col: 1 }),
    (KeyCode::KEY_2, MatrixPos { row: 1, col: 2 }),
    (KeyCode::KEY_3, MatrixPos { row: 1, col: 3 }),
    (KeyCode::KEY_4, MatrixPos { row: 1, col: 4 }),
    (KeyCode::KEY_5, MatrixPos { row: 1, col: 5 }),
    (KeyCode::KEY_6, MatrixPos { row: 1, col: 6 }),
    (KeyCode::KEY_7, MatrixPos { row: 1, col: 7 }),
    (KeyCode::KEY_8, MatrixPos { row: 1, col: 8 }),
    (KeyCode::KEY_9, MatrixPos { row: 1, col: 9 }),
    (KeyCode::KEY_0, MatrixPos { row: 1, col: 10 }),
    (KeyCode::KEY_MINUS, MatrixPos { row: 1, col: 11 }),
    (KeyCode::KEY_EQUAL, MatrixPos { row: 1, col: 12 }),
    (KeyCode::KEY_Q, MatrixPos { row: 2, col: 1 }),
    (KeyCode::KEY_W, MatrixPos { row: 2, col: 2 }),
    (KeyCode::KEY_E, MatrixPos { row: 2, col: 3 }),
    (KeyCode::KEY_R, MatrixPos { row: 2, col: 4 }),
    (KeyCode::KEY_T, MatrixPos { row: 2, col: 5 }),
    (KeyCode::KEY_Y, MatrixPos { row: 2, col: 6 }),
    (KeyCode::KEY_U, MatrixPos { row: 2, col: 7 }),
    (KeyCode::KEY_I, MatrixPos { row: 2, col: 8 }),
    (KeyCode::KEY_O, MatrixPos { row: 2, col: 9 }),
    (KeyCode::KEY_P, MatrixPos { row: 2, col: 10 }),
    (KeyCode::KEY_LEFTBRACE, MatrixPos { row: 2, col: 11 }),
    (KeyCode::KEY_RIGHTBRACE, MatrixPos { row: 2, col: 12 }),
    (KeyCode::KEY_A, MatrixPos { row: 3, col: 1 }),
    (KeyCode::KEY_S, MatrixPos { row: 3, col: 2 }),
    (KeyCode::KEY_D, MatrixPos { row: 3, col: 3 }),
    (KeyCode::KEY_F, MatrixPos { row: 3, col: 4 }),
    (KeyCode::KEY_G, MatrixPos { row: 3, col: 5 }),
    (KeyCode::KEY_H, MatrixPos { row: 3, col: 6 }),
    (KeyCode::KEY_J, MatrixPos { row: 3, col: 7 }),
    (KeyCode::KEY_K, MatrixPos { row: 3, col: 8 }),
    (KeyCode::KEY_L, MatrixPos { row: 3, col: 9 }),
    (KeyCode::KEY_SEMICOLON, MatrixPos { row: 3, col: 10 }),
    (KeyCode::KEY_APOSTROPHE, MatrixPos { row: 3, col: 11 }),
    // Row 4 has no switch at column 1 (the wide left shift is at 0) or 11.
    (KeyCode::KEY_Z, MatrixPos { row: 4, col: 2 }),
    (KeyCode::KEY_X, MatrixPos { row: 4, col: 3 }),
    (KeyCode::KEY_C, MatrixPos { row: 4, col: 4 }),
    (KeyCode::KEY_V, MatrixPos { row: 4, col: 5 }),
    (KeyCode::KEY_B, MatrixPos { row: 4, col: 6 }),
    (KeyCode::KEY_N, MatrixPos { row: 4, col: 7 }),
    (KeyCode::KEY_M, MatrixPos { row: 4, col: 8 }),
    (KeyCode::KEY_COMMA, MatrixPos { row: 4, col: 9 }),
    (KeyCode::KEY_DOT, MatrixPos { row: 4, col: 10 }),
    (KeyCode::KEY_SLASH, MatrixPos { row: 4, col: 12 }),
    (KeyCode::KEY_LEFTSHIFT, MatrixPos { row: 4, col: 0 }),
    (KeyCode::KEY_RIGHTSHIFT, MatrixPos { row: 4, col: 13 }),
];

pub fn matrix_pos(key: KeyCode) -> Option<MatrixPos> {
    KEYS.iter().find(|&&(k, _)| k == key).map(|&(_, p)| p)
}

/// The key at a matrix position, if it's one we use.
pub fn key_at(pos: MatrixPos) -> Option<KeyCode> {
    KEYS.iter().find(|&&(_, p)| p == pos).map(|&(k, _)| k)
}

/// One travel reading, timestamped at the midpoint of the request/reply round
/// trip (the firmware reads the value somewhere in between).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Sample {
    pub time: SystemTime,
    pub travel: Travel,
    /// The raw 12-bit Hall sensor reading behind `travel` (lower = deeper).
    pub adc: u16,
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
            adc: u16::from_le_bytes([reply[7], reply[8]]),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn positions_are_distinct_and_in_range() {
        let positions: HashSet<_> = KEYS.iter().map(|&(_, p)| (p.row, p.col)).collect();
        let keys: HashSet<_> = KEYS.iter().map(|&(k, _)| k).collect();
        assert_eq!(positions.len(), KEYS.len());
        assert_eq!(keys.len(), KEYS.len());
        assert!(KEYS.iter().all(|&(_, p)| p.row < ROWS && p.col < COLS));
        for &(k, p) in KEYS {
            assert_eq!(key_at(p), Some(k));
        }
    }
}
