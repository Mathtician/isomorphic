//! Measures how fast one Keychron HE key's analog travel can be polled over raw
//! HID, and records a travel trace while you press it.

use std::time::{Duration, SystemTime};

use clap::Parser;
use isomorphic::keychron::{self, Hid, MatrixPos};

#[derive(Parser)]
struct Args {
    /// Matrix row (e.g. 3 for the A..' row)
    row: u8,
    /// Matrix column (e.g. 4 for F)
    col: u8,
    /// How long to poll, in seconds
    #[arg(long, default_value_t = 5.0)]
    seconds: f64,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let mut hid = Hid::open(keychron::HIDRAW)?;
    let pos = MatrixPos {
        row: args.row,
        col: args.col,
    };

    let start = SystemTime::now();
    let mut round_trips = Vec::new();
    let mut trace: Vec<(Duration, u8)> = Vec::new();
    println!(
        "Polling {pos:?} for {}s; press the key a few times.",
        args.seconds
    );
    while start.elapsed()?.as_secs_f64() < args.seconds {
        let sent = SystemTime::now();
        let sample = hid.travel(pos)?;
        round_trips.push(sent.elapsed()?);
        if trace.last().is_none_or(|&(_, t)| t != sample.travel) {
            trace.push((sample.time.duration_since(start)?, sample.travel));
        }
    }

    round_trips.sort();
    let pct = |p: f64| round_trips[((round_trips.len() - 1) as f64 * p) as usize];
    println!(
        "{} polls: round trip min {:?}, median {:?}, p99 {:?}, max {:?}",
        round_trips.len(),
        pct(0.0),
        pct(0.5),
        pct(0.99),
        pct(1.0),
    );
    println!("\ntravel trace (only changes; travel in 1/60 mm, 240 = 4.0 mm):");
    for (t, travel) in trace {
        println!(
            "{:9.3} ms  {travel:3}  {}",
            t.as_secs_f64() * 1000.0,
            "#".repeat(travel as usize / 4)
        );
    }
    Ok(())
}
