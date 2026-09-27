//! Turns a Keychron HE key's analog travel into a MIDI velocity.
//!
//! A key's ordinary key-down event marks the moment it crossed its actuation
//! point (set in Keychron Launcher; it must be shallower than the strike depth).
//! From then on we poll the key's travel until it passes the strike depth. The
//! note sounds then, like a piano hammer hitting the string, and its velocity
//! comes from how long the key took to get from actuation to strike depth. A
//! key released before reaching the strike depth plays nothing.

use std::{
    error::Error,
    time::{Duration, SystemTime},
};

use evdev::KeyCode;

use crate::keychron::{Hid, MatrixPos, Sample, Travel};

/// Warn about a key whose travel reads 0 this many times in a row while its
/// key-down says it's pressed: its matrix position is probably wrong.
const REST_POLLS_BEFORE_WARNING: u32 = 5;

/// Maps descent time to velocity on a log scale, so each halving of the time
/// adds the same amount of velocity.
#[derive(Clone, Copy, Debug)]
pub struct Curve {
    fast: Duration,
    slow: Duration,
    /// Velocity at `slow`, 1..=127.
    floor: u8,
}

impl Curve {
    pub fn new(fast: Duration, slow: Duration, floor: u8) -> Result<Curve, String> {
        if !(1..=127).contains(&floor) {
            return Err(format!("velocity floor {floor} not in 1..=127"));
        }
        if Duration::ZERO < fast && fast < slow {
            Ok(Curve { fast, slow, floor })
        } else {
            Err(format!(
                "need 0 < fast < slow, got fast {fast:?}, slow {slow:?}"
            ))
        }
    }

    /// `fast` or quicker gives 127; `slow` or slower gives `floor`.
    pub fn velocity(&self, descent: Duration) -> u8 {
        let d = descent.clamp(self.fast, self.slow).as_secs_f64();
        let (fast, slow) = (self.fast.as_secs_f64(), self.slow.as_secs_f64());
        let slowness = (d / fast).ln() / (slow / fast).ln(); // 0 at fast, 1 at slow
        (127.0 - (127 - self.floor) as f64 * slowness).round() as u8
    }
}

/// When the key crossed `depth`, given the first sample at or past it and the
/// sample before it (if any), interpolating linearly between the two. Without a
/// previous sample the key went from actuation past `depth` before our first
/// poll, and the best we know is that it had crossed by `cur.time`.
fn crossing(prev: Option<Sample>, cur: Sample, depth: Travel) -> SystemTime {
    assert!(cur.travel >= depth);
    let Some(prev) = prev else { return cur.time };
    assert!(prev.travel < depth);
    let fraction = (depth - prev.travel) as f64 / (cur.travel - prev.travel) as f64;
    let gap = cur.time.duration_since(prev.time).unwrap_or_default();
    prev.time + gap.mul_f64(fraction)
}

/// A key pressed past actuation that hasn't reached strike depth yet.
struct Descent {
    key: KeyCode,
    pos: MatrixPos,
    pressed_at: SystemTime,
    /// Latest sample, always shallower than the strike depth.
    last: Option<Sample>,
    rest_polls: u32,
}

/// A key that has reached strike depth: time to play its note.
#[derive(Debug)]
pub struct Strike {
    pub key: KeyCode,
    pub velocity: u8,
    /// Time from actuation to strike depth.
    pub descent: Duration,
}

pub struct Measurer {
    hid: Hid,
    curve: Curve,
    strike_depth: Travel,
    descending: Vec<Descent>,
    /// Round-robin cursor into `descending`.
    next: usize,
}

impl Measurer {
    pub fn new(hid: Hid, curve: Curve, strike_depth: Travel) -> Measurer {
        Measurer {
            hid,
            curve,
            strike_depth,
            descending: Vec::new(),
            next: 0,
        }
    }

    /// A key crossed its actuation point at `at` (the key-down's timestamp).
    pub fn press(&mut self, key: KeyCode, pos: MatrixPos, at: SystemTime) {
        if self.descending.iter().all(|d| d.key != key) {
            self.descending.push(Descent {
                key,
                pos,
                pressed_at: at,
                last: None,
                rest_polls: 0,
            });
        }
    }

    /// A key came up; if it never reached strike depth, it never sounds.
    pub fn release(&mut self, key: KeyCode) {
        self.descending.retain(|d| d.key != key);
    }

    /// Whether no key is waiting to be polled.
    pub fn is_idle(&self) -> bool {
        self.descending.is_empty()
    }

    /// Polls the next descending key (one USB round trip, about 2 ms). Returns
    /// a strike if that key has now reached strike depth.
    pub fn poll(&mut self) -> Result<Option<Strike>, Box<dyn Error>> {
        assert!(!self.is_idle(), "poll with no key descending");
        self.next %= self.descending.len();
        let d = &mut self.descending[self.next];
        let sample = self.hid.travel(d.pos)?;

        if sample.travel >= self.strike_depth {
            let at = crossing(d.last, sample, self.strike_depth);
            // Can only fail if the wall clock jumps backwards mid-keystroke.
            let descent = at.duration_since(d.pressed_at).unwrap_or_default();
            let strike = Strike {
                key: d.key,
                velocity: self.curve.velocity(descent),
                descent,
            };
            self.descending.remove(self.next);
            return Ok(Some(strike));
        }

        d.rest_polls = if sample.travel == 0 {
            d.rest_polls + 1
        } else {
            0
        };
        if d.rest_polls == REST_POLLS_BEFORE_WARNING {
            eprintln!(
                "warning: {:?} is down but its travel at {:?} reads 0; wrong matrix position?",
                d.key, d.pos
            );
        }
        d.last = Some(sample);
        self.next += 1;
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: f64) -> Duration {
        Duration::from_secs_f64(n / 1000.0)
    }

    fn sample(t_ms: f64, travel: Travel) -> Sample {
        Sample {
            time: SystemTime::UNIX_EPOCH + ms(t_ms),
            travel,
        }
    }

    #[test]
    fn curve_endpoints_and_log_midpoint() {
        let c = Curve::new(ms(2.0), ms(32.0), 1).unwrap();
        assert_eq!(c.velocity(ms(0.5)), 127);
        assert_eq!(c.velocity(ms(2.0)), 127);
        assert_eq!(c.velocity(ms(8.0)), 64); // geometric midpoint of 2 and 32
        assert_eq!(c.velocity(ms(32.0)), 1);
        assert_eq!(c.velocity(ms(1000.0)), 1);
        assert!(Curve::new(ms(5.0), ms(5.0), 1).is_err());
        assert!(Curve::new(ms(2.0), ms(32.0), 0).is_err());

        let c = Curve::new(ms(2.0), ms(32.0), 31).unwrap();
        assert_eq!(c.velocity(ms(2.0)), 127);
        assert_eq!(c.velocity(ms(8.0)), 79); // halfway between 127 and 31
        assert_eq!(c.velocity(ms(32.0)), 31);
        assert_eq!(c.velocity(ms(1000.0)), 31);
    }

    #[test]
    fn crossing_interpolates() {
        // From 100 to 200 over 4 ms: 180 is crossed 80% of the way.
        let t = crossing(Some(sample(10.0, 100)), sample(14.0, 200), 180);
        assert_eq!(t, SystemTime::UNIX_EPOCH + ms(13.2));
        // Landing exactly on the depth counts as crossing then.
        let t = crossing(Some(sample(10.0, 100)), sample(14.0, 180), 180);
        assert_eq!(t, SystemTime::UNIX_EPOCH + ms(14.0));
        // No earlier sample: all we know is it had crossed by the first one.
        assert_eq!(
            crossing(None, sample(3.0, 245), 180),
            SystemTime::UNIX_EPOCH + ms(3.0)
        );
    }
}
