//! Measures how fast one Keychron HE key's analog travel can be polled over raw
//! HID, and records a travel trace while you press it.
//!
//! Protocol (Keychron qmk_firmware, branch 2025q3, common/analog_matrix/analog_matrix.c):
//! request `A9 30 row col`; reply `A9 30 status row col travel/6 travel ...`,
//! where travel is in 1/60 mm, 0..=245. One key per request; nothing is streamed.

use std::{
    fs::OpenOptions,
    io::{Read, Write},
    time::{Duration, Instant},
};

use clap::Parser;

const REPORT_LEN: usize = 32;

#[derive(Parser)]
struct Args {
    /// Matrix row (e.g. 3 for the A..' row)
    row: u8,
    /// Matrix column (e.g. 4 for F)
    col: u8,
    /// Raw HID device of the VIA interface (usage page 0xFF60)
    #[arg(long, default_value = "/dev/hidraw7")]
    hidraw: String,
    /// How long to poll, in seconds
    #[arg(long, default_value_t = 5.0)]
    seconds: f64,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let mut dev = OpenOptions::new().read(true).write(true).open(&args.hidraw)?;

    // hidraw write: leading report ID 0 (the interface has none), then the report.
    let mut request = [0u8; 1 + REPORT_LEN];
    request[1..5].copy_from_slice(&[0xA9, 0x30, args.row, args.col]);

    let start = Instant::now();
    let mut round_trips = Vec::new();
    let mut trace: Vec<(Duration, u8)> = Vec::new();
    let mut skipped = 0;
    println!("Polling row {} col {} for {}s; press the key a few times.", args.row, args.col, args.seconds);
    while start.elapsed().as_secs_f64() < args.seconds {
        let sent = Instant::now();
        dev.write_all(&request)?;
        let reply = loop {
            let mut buf = [0u8; REPORT_LEN];
            let n = dev.read(&mut buf)?;
            assert_eq!(n, REPORT_LEN, "short hidraw read");
            // Skip unsolicited reports (e.g. layer-change notifications).
            if buf[..2] == [0xA9, 0x30] && buf[3..5] == [args.row, args.col] {
                break buf;
            }
            skipped += 1;
        };
        let now = Instant::now();
        if reply[2] != 0 {
            return Err(format!("firmware returned error status {} (bad row/col?)", reply[2]).into());
        }
        round_trips.push(now - sent);
        let travel = reply[6];
        if trace.last().is_none_or(|&(_, t)| t != travel) {
            trace.push((now - start, travel));
        }
    }

    round_trips.sort();
    let pct = |p: f64| round_trips[((round_trips.len() - 1) as f64 * p) as usize];
    println!(
        "{} polls: round trip min {:?}, median {:?}, p99 {:?}, max {:?}; {} unrelated reports skipped",
        round_trips.len(),
        pct(0.0),
        pct(0.5),
        pct(0.99),
        pct(1.0),
        skipped
    );
    println!("\ntravel trace (only changes; travel in 1/60 mm, 240 = 4.0 mm):");
    for (t, travel) in trace {
        println!("{:9.3} ms  {travel:3}  {}", t.as_secs_f64() * 1000.0, "#".repeat(travel as usize / 4));
    }
    Ok(())
}
