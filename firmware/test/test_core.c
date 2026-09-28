// Host unit tests for instrument_core. Run: make -C test
#include <assert.h>
#include <stdio.h>
#include <string.h>

#include "instrument_core.h"

// 1 timer tick = 1 µs, so a descent unit (10 µs) is 10 ticks.
#define TICKS_PER_UNIT 10
static const ic_config_t CFG  = {.start = 30, .strike = 180, .release = 60, .pressure_step = 3};
static const uint16_t    ALL[IC_ROWS] = {0x7FFF, 0x7FFF, 0x7FFF, 0x7FFF, 0x7FFF, 0x7FFF};

static ic_t    ic;
static uint8_t travel[IC_ROWS][IC_COLS];

static void setup(uint32_t now) {
    memset(travel, 0, sizeof travel);
    ic_init(&ic, CFG, TICKS_PER_UNIT, now, travel);
}

static void scan(uint32_t now, uint8_t row, uint8_t col, uint8_t t) {
    travel[row][col] = t;
    ic_scan(&ic, now, travel, ALL);
}

static uint8_t take(ic_event_t *out, uint8_t max) {
    bool overflowed;
    uint8_t n = ic_take_events(&ic, out, max, &overflowed);
    assert(!overflowed);
    return n;
}

// Only note events, dropping pressure reports.
static uint8_t take_notes(ic_event_t *out) {
    ic_event_t all[IC_QUEUE_LEN + IC_ROWS * IC_COLS];
    uint8_t    n = take(all, sizeof all / sizeof *all), m = 0;
    for (uint8_t i = 0; i < n; i++)
        if (all[i].type != IC_EV_PRESSURE) out[m++] = all[i];
    return m;
}

static void test_slow_press(void) {
    setup(0);
    ic_event_t ev[8];
    // 0 → 20 → 40 (start 30 crossed at t = 1000 + 10/20 * 1000 = 1500) ...
    scan(1000, 2, 3, 20);
    scan(2000, 2, 3, 40);
    scan(3000, 2, 3, 100);
    assert(take_notes(ev) == 0);
    // ... → 200 (strike 180 crossed at t = 3000 + 80/100 * 1000 = 3800).
    scan(4000, 2, 3, 200);
    assert(take_notes(ev) == 1);
    assert(ev[0].type == IC_EV_NOTE_ON && ev[0].row == 2 && ev[0].col == 3);
    assert(ev[0].value == (3800 - 1500) / TICKS_PER_UNIT);
}

static void test_both_depths_in_one_scan(void) {
    setup(0);
    ic_event_t ev[8];
    // 0 → 240 in one 1000 µs scan: start at 30/240, strike at 180/240 of it.
    scan(1000, 0, 0, 240);
    assert(take_notes(ev) == 1);
    assert(ev[0].type == IC_EV_NOTE_ON);
    assert(ev[0].value == (750 - 125) / TICKS_PER_UNIT);
}

static void test_abort_is_silent(void) {
    setup(0);
    ic_event_t ev[8];
    scan(1000, 1, 1, 100);
    scan(2000, 1, 1, 10);
    scan(3000, 1, 1, 0);
    assert(take_notes(ev) == 0);
    // Pressed properly afterwards, it measures from the new start crossing.
    scan(4000, 1, 1, 30);
    scan(5000, 1, 1, 180);
    assert(take_notes(ev) == 1 && ev[0].value == 1000 / TICKS_PER_UNIT);
}

static void test_release_and_rearm(void) {
    setup(0);
    ic_event_t ev[8];
    scan(1000, 3, 4, 240);
    assert(take_notes(ev) == 1);
    scan(2000, 3, 4, 61); // above release: still sounding
    assert(take_notes(ev) == 0);
    scan(3000, 3, 4, 59); // below release: note-off, but not yet below start
    assert(take_notes(ev) == 1 && ev[0].type == IC_EV_NOTE_OFF && ev[0].row == 3 && ev[0].col == 4);
    scan(4000, 3, 4, 240); // pressed again without letting up past start: silent
    assert(take_notes(ev) == 0);
    scan(5000, 3, 4, 20); // let up past start ...
    scan(6000, 3, 4, 240); // ... so this strikes
    assert(take_notes(ev) == 1 && ev[0].type == IC_EV_NOTE_ON);
}

static void test_held_at_init(void) {
    memset(travel, 0, sizeof travel);
    travel[4][2] = 240;
    ic_init(&ic, CFG, TICKS_PER_UNIT, 0, travel);
    ic_event_t ev[8];
    scan(1000, 4, 2, 240);
    scan(2000, 4, 2, 50);
    assert(take_notes(ev) == 0); // no note-off for a note never started
    scan(3000, 4, 2, 0);
    scan(4000, 4, 2, 240);
    assert(take_notes(ev) == 1 && ev[0].type == IC_EV_NOTE_ON);
}

static void test_pressure(void) {
    setup(0);
    ic_event_t ev[8];
    scan(1000, 2, 2, 200);
    // Note-on comes first, then the key's current travel as pressure.
    assert(take(ev, 8) == 2);
    assert(ev[0].type == IC_EV_NOTE_ON);
    assert(ev[1].type == IC_EV_PRESSURE && ev[1].value == 200);
    scan(2000, 2, 2, 202); // less than pressure_step: nothing
    assert(take(ev, 8) == 0);
    scan(3000, 2, 2, 197); // 3 below the last report
    assert(take(ev, 8) == 1 && ev[0].type == IC_EV_PRESSURE && ev[0].value == 197);
    // Note events take precedence over pressure when space is short.
    scan(4000, 5, 5, 240); // a second key strikes
    assert(take(ev, 1) == 1 && ev[0].type == IC_EV_NOTE_ON && ev[0].row == 5);
    assert(take(ev, 8) == 1 && ev[0].type == IC_EV_PRESSURE && ev[0].row == 5);
}

static void test_pressure_round_robin(void) {
    setup(0);
    ic_event_t ev[8];
    travel[0][0] = travel[0][1] = 240;
    ic_scan(&ic, 1000, travel, ALL);
    take(ev, 2); // both note-ons
    take(ev, 1); // pressure for (0,0)
    assert(ev[0].row == 0 && ev[0].col == 0);
    travel[0][0] = 200;
    ic_scan(&ic, 2000, travel, ALL);
    // (0,1) hasn't had its first report yet, so it goes before (0,0)'s second.
    assert(take(ev, 1) == 1 && ev[0].col == 1);
    assert(take(ev, 1) == 1 && ev[0].col == 0 && ev[0].value == 200);
}

static void test_overflow(void) {
    setup(0);
    ic_event_t ev[IC_QUEUE_LEN + 1];
    uint32_t   now = 0;
    // Strike and release one key more times than the queue holds.
    for (int i = 0; i < IC_QUEUE_LEN; i++) {
        scan(now += 1000, 1, 2, 240);
        scan(now += 1000, 1, 2, 0);
    }
    bool    overflowed;
    uint8_t n = ic_take_events(&ic, ev, IC_QUEUE_LEN + 1, &overflowed);
    assert(overflowed && n == IC_QUEUE_LEN);
    // The events kept are the oldest, in order.
    assert(ev[0].type == IC_EV_NOTE_ON && ev[1].type == IC_EV_NOTE_OFF);
    ic_take_events(&ic, ev, 1, &overflowed);
    assert(!overflowed); // reported once
}

static void test_timer_wrap(void) {
    setup(UINT32_MAX - 499);
    ic_event_t ev[8];
    // Same geometry as test_both_depths_in_one_scan, straddling the wrap.
    scan(500, 0, 0, 240);
    assert(take_notes(ev) == 1 && ev[0].value == (750 - 125) / TICKS_PER_UNIT);
}

static void test_stale_descent(void) {
    setup(0);
    ic_event_t ev[8];
    uint32_t   now = 0;
    scan(now += 1000, 0, 5, 100); // parked halfway ...
    // ... for longer than a u16 of descent units, in 1 ms scans.
    for (uint32_t i = 0; i < (uint32_t)IC_DESCENT_MAX * TICKS_PER_UNIT / 1000 + 2; i++) scan(now += 1000, 0, 5, 100);
    scan(now += 1000, 0, 5, 240);
    assert(take_notes(ev) == 1 && ev[0].value == IC_DESCENT_MAX);
}

static void test_mask(void) {
    setup(0);
    uint16_t mask[IC_ROWS] = {0};
    mask[2]                = 1u << 7;
    ic_event_t ev[8];
    travel[2][6] = travel[2][7] = 240;
    ic_scan(&ic, 1000, travel, mask);
    assert(take_notes(ev) == 1 && ev[0].row == 2 && ev[0].col == 7);
}

static void test_config_validation(void) {
    assert(ic_config_valid(CFG));
    assert(!ic_config_valid((ic_config_t){.start = 0, .strike = 180, .release = 60, .pressure_step = 3}));
    assert(!ic_config_valid((ic_config_t){.start = 180, .strike = 180, .release = 60, .pressure_step = 3}));
    assert(!ic_config_valid((ic_config_t){.start = 30, .strike = 180, .release = 181, .pressure_step = 3}));
    assert(!ic_config_valid((ic_config_t){.start = 30, .strike = 180, .release = 0, .pressure_step = 3}));
    assert(!ic_config_valid((ic_config_t){.start = 30, .strike = 180, .release = 60, .pressure_step = 0}));
    // release below start is fine: note-off then goes straight to REST.
    assert(ic_config_valid((ic_config_t){.start = 30, .strike = 180, .release = 10, .pressure_step = 3}));
}

int main(void) {
    test_slow_press();
    test_both_depths_in_one_scan();
    test_abort_is_silent();
    test_release_and_rearm();
    test_held_at_init();
    test_pressure();
    test_pressure_round_robin();
    test_overflow();
    test_timer_wrap();
    test_stale_descent();
    test_mask();
    test_config_validation();
    puts("all instrument_core tests passed");
    return 0;
}
