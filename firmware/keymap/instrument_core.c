#include "instrument_core.h"

bool ic_config_valid(ic_config_t cfg) {
    return 0 < cfg.start && cfg.start < cfg.strike && 0 < cfg.release && cfg.release <= cfg.strike && cfg.pressure_step > 0;
}

void ic_init(ic_t *ic, ic_config_t cfg, uint32_t ticks_per_unit, uint32_t now, const uint8_t travel[IC_ROWS][IC_COLS]) {
    ic->cfg            = cfg;
    ic->ticks_per_unit = ticks_per_unit;
    ic->prev_time      = now;
    ic->queue_head     = 0;
    ic->queue_len      = 0;
    ic->overflowed     = false;
    ic->pressure_next  = 0;
    for (uint8_t r = 0; r < IC_ROWS; r++) {
        for (uint8_t c = 0; c < IC_COLS; c++) {
            ic_key_t *k = &ic->keys[r][c];
            k->travel   = travel[r][c];
            k->state    = travel[r][c] < cfg.start ? IC_REST : IC_RELEASED;
        }
    }
}

static void enqueue(ic_t *ic, uint8_t type, uint8_t row, uint8_t col, uint16_t value) {
    if (ic->queue_len == IC_QUEUE_LEN) {
        ic->overflowed = true;
        return;
    }
    uint8_t tail    = (ic->queue_head + ic->queue_len) % IC_QUEUE_LEN;
    ic->queue[tail] = (ic_event_t){.type = type, .row = row, .col = col, .value = value};
    ic->queue_len++;
}

// When travel crossed `threshold`, given readings `before` at t0 and `after`
// at t1 with before < threshold ≤ after.
static uint32_t crossing(uint8_t before, uint8_t after, uint8_t threshold, uint32_t t0, uint32_t t1) {
    uint32_t span = t1 - t0; // modulo 2^32, correct across a timer wrap
    return t0 + (uint32_t)((uint64_t)(threshold - before) * span / (uint8_t)(after - before));
}

static uint16_t descent_units(const ic_t *ic, uint32_t ticks) {
    uint32_t units = ticks / ic->ticks_per_unit;
    return units > IC_DESCENT_MAX ? IC_DESCENT_MAX : (uint16_t)units;
}

static void step_key(ic_t *ic, uint8_t row, uint8_t col, uint8_t travel, uint32_t now) {
    ic_key_t         *k   = &ic->keys[row][col];
    const ic_config_t cfg = ic->cfg;
    uint8_t           was = k->travel;
    k->travel             = travel;

    switch (k->state) {
        case IC_REST: // was < start
            if (travel < cfg.start) break;
            k->t_start     = crossing(was, travel, cfg.start, ic->prev_time, now);
            k->start_stale = false;
            k->state       = IC_DESCENDING;
            // A fast key can pass both depths between two scans: handle the
            // strike right away, measured from the interpolated start.
            if (travel < cfg.strike) break;
            // fall through
        case IC_DESCENDING: // start ≤ was < strike, or just crossed start above
            if (travel < cfg.start) {
                k->state = IC_REST;
            } else if (travel >= cfg.strike) {
                // `was` is below strike either way: DESCENDING's invariant, or
                // below start when falling through from REST.
                uint32_t t_strike = crossing(was, travel, cfg.strike, ic->prev_time, now);
                uint16_t descent  = k->start_stale ? IC_DESCENT_MAX : descent_units(ic, t_strike - k->t_start);
                enqueue(ic, IC_EV_NOTE_ON, row, col, descent);
                k->sent_pressure = 0; // so the key's pressure gets reported promptly
                k->state         = IC_STRUCK;
            } else if (!k->start_stale && descent_units(ic, now - k->t_start) == IC_DESCENT_MAX) {
                // Stop measuring before the timer could wrap past t_start.
                k->start_stale = true;
            }
            break;
        case IC_STRUCK: // was ≥ release
            if (travel < cfg.release) {
                enqueue(ic, IC_EV_NOTE_OFF, row, col, 0);
                k->state = travel < cfg.start ? IC_REST : IC_RELEASED;
            }
            break;
        case IC_RELEASED: // was ≥ start
            if (travel < cfg.start) k->state = IC_REST;
            break;
    }
}

void ic_scan(ic_t *ic, uint32_t now, const uint8_t travel[IC_ROWS][IC_COLS], const uint16_t mask[IC_ROWS]) {
    for (uint8_t r = 0; r < IC_ROWS; r++) {
        for (uint8_t c = 0; c < IC_COLS; c++) {
            if (mask[r] & (1u << c)) step_key(ic, r, c, travel[r][c], now);
        }
    }
    ic->prev_time = now;
}

uint8_t ic_take_events(ic_t *ic, ic_event_t *out, uint8_t max, bool *overflowed) {
    uint8_t n = 0;
    for (; n < max && ic->queue_len > 0; n++) {
        out[n]         = ic->queue[ic->queue_head];
        ic->queue_head = (ic->queue_head + 1) % IC_QUEUE_LEN;
        ic->queue_len--;
    }

    const uint16_t n_keys = IC_ROWS * IC_COLS;
    for (uint16_t i = 0; i < n_keys && n < max; i++) {
        uint16_t  idx = (ic->pressure_next + i) % n_keys;
        uint8_t   r = idx / IC_COLS, c = idx % IC_COLS;
        ic_key_t *k = &ic->keys[r][c];
        if (k->state != IC_STRUCK) continue;
        uint8_t moved = k->travel > k->sent_pressure ? k->travel - k->sent_pressure : k->sent_pressure - k->travel;
        if (moved < ic->cfg.pressure_step) continue;
        out[n++]          = (ic_event_t){.type = IC_EV_PRESSURE, .row = r, .col = c, .value = k->travel};
        k->sent_pressure  = k->travel;
        ic->pressure_next = (idx + 1) % n_keys;
    }

    *overflowed    = ic->overflowed;
    ic->overflowed = false;
    return n;
}
