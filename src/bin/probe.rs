//! Records one Keychron HE key's analog travel over raw HID while you press it:
//! how fast it can be polled, and a trace of travel and the raw sensor reading.

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
    /// Also log changes of at least this much in the raw sensor reading
    /// (useful at full travel, where travel itself is clamped)
    #[arg(long, default_value_t = 3)]
    adc_step: u16,
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
    let mut trace: Vec<(Duration, u8, u16)> = Vec::new();
    println!(
        "Polling {pos:?} for {}s; press the key a few times.",
        args.seconds
    );
    while start.elapsed()?.as_secs_f64() < args.seconds {
        let sent = SystemTime::now();
        let s = hid.travel(pos)?;
        round_trips.push(sent.elapsed()?);
        let changed = trace.last().is_none_or(|&(_, travel, adc)| {
            travel != s.travel || adc.abs_diff(s.adc) >= args.adc_step
        });
        if changed {
            trace.push((s.time.duration_since(start)?, s.travel, s.adc));
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
    println!("\ntime (ms)  travel (1/60 mm, 240 = 4.0 mm)  raw sensor");
    for (t, travel, adc) in trace {
        println!(
            "{:9.3}  {travel:3} {:<61}  {adc}",
            t.as_secs_f64() * 1000.0,
            "#".repeat(travel as usize / 4)
        );
    }
    Ok(())
}
