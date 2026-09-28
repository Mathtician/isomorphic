// Platform-independent core of instrument mode: per-key state machines that
// turn analog key travel into note-on (with descent time), note-off and
// pressure events, plus the queue those events wait in. No QMK dependencies,
// so it can be unit-tested on the host (see ../test/test_core.c).
//
// Each key moves through these states, judged once per matrix scan:
//
//   REST ──travel ≥ start──▶ DESCENDING ──travel ≥ strike──▶ STRUCK (note-on)
//    ▲                           │                              │
//    └──────travel < start───────┘                     travel < release (note-off)
//    ▲                                                          │
//    └──travel < start── RELEASED ◀──travel still ≥ start───────┘
//
// A strike's descent time runs from the moment the key crossed `start` to the
// moment it crossed `strike`, each interpolated linearly between the two scans
// that straddle the crossing. Both crossings can fall between the same pair of
// scans; the interpolation then still gives a finite time from the slope.
//
// Invariants, with `travel` the key's latest reading:
//   REST       ⇒ travel < start
//   DESCENDING ⇒ start ≤ travel < strike
//   STRUCK     ⇒ travel ≥ release
//   RELEASED   ⇒ travel ≥ start
// A key has to come back above `start` (travel < start) before it can strike
// again, so a key that is let up only partway and pressed again stays silent.

#pragma once

#include <stdbool.h>
#include <stdint.h>

#ifndef IC_ROWS
#    define IC_ROWS 6
#endif
#ifndef IC_COLS
#    define IC_COLS 15
#endif
#define IC_QUEUE_LEN 64

// Descent times are reported in units of 10 µs, saturating at the u16 limit
// (0.65535 s): slow enough that any velocity curve has bottomed out.
#define IC_DESCENT_UNIT_US 10
#define IC_DESCENT_MAX 0xFFFF

typedef struct {
    uint8_t start;         // travel (1/60 mm) at which descent timing starts
    uint8_t strike;        // travel at which the note sounds
    uint8_t release;       // a struck key's note stops when travel falls below this
    uint8_t pressure_step; // min travel change for a new pressure report
} ic_config_t;

typedef enum {
    IC_EV_NOTE_ON  = 1, // value: descent time in IC_DESCENT_UNIT_US units
    IC_EV_NOTE_OFF = 2, // value: 0
    IC_EV_PRESSURE = 3, // value: current travel
} ic_event_type_t;

typedef struct {
    uint8_t  type;
    uint8_t  row;
    uint8_t  col;
    uint16_t value;
} ic_event_t;

typedef enum { IC_REST, IC_DESCENDING, IC_STRUCK, IC_RELEASED } ic_key_state_t;

typedef struct {
    uint8_t  state;         // ic_key_state_t
    uint8_t  travel;        // reading at the previous scan
    uint8_t  sent_pressure; // STRUCK only: travel in the last pressure event
    bool     start_stale;   // DESCENDING only: t_start is too old to measure from
    uint32_t t_start;       // DESCENDING only: when `start` was crossed
} ic_key_t;

typedef struct {
    ic_config_t cfg;
    uint32_t    ticks_per_unit; // timer ticks per IC_DESCENT_UNIT_US
    uint32_t    prev_time;      // timer at the previous scan
    ic_key_t    keys[IC_ROWS][IC_COLS];
    ic_event_t  queue[IC_QUEUE_LEN];
    uint8_t     queue_head, queue_len;
    bool        overflowed;    // a note event was dropped since the last report
    uint16_t    pressure_next; // round-robin cursor over keys for pressure events
} ic_t;

// Whether `cfg` makes sense: 0 < start < strike, 0 < release ≤ strike, pressure_step > 0.
bool ic_config_valid(ic_config_t cfg);

// Starts tracking from the current readings. Keys already at or past `start`
// begin RELEASED, so keys held while instrument mode starts don't sound until
// they have been let up. `ticks_per_unit` is the timer rate per 10 µs.
void ic_init(ic_t *ic, ic_config_t cfg, uint32_t ticks_per_unit, uint32_t now, const uint8_t travel[IC_ROWS][IC_COLS]);

// Advances every key in `mask` (bit c of mask[r] set ⇒ key r,c exists) with
// the readings of a scan finished at timer value `now`. The timer may wrap;
// intervals are computed modulo 2^32, so scans must be less than a wrap apart.
void ic_scan(ic_t *ic, uint32_t now, const uint8_t travel[IC_ROWS][IC_COLS], const uint16_t mask[IC_ROWS]);

// Fills `out` with up to `max` events: queued note events first, in order,
// then pressure events for struck keys whose travel moved at least
// `pressure_step` since their last one. Returns how many were written and
// sets *overflowed if note events were lost since the previous call.
uint8_t ic_take_events(ic_t *ic, ic_event_t *out, uint8_t max, bool *overflowed);
