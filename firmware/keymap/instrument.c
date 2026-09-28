// Instrument mode: the QMK side. While a host program keeps it active, the
// keyboard stops sending keystrokes, reports every key's strikes, releases and
// pressure over raw HID (see instrument_core.h for how those are judged), and
// lights keys in host-chosen colours. Everything else about the keyboard is
// stock, and it falls back to being a normal keyboard if the host goes quiet.
//
// Host → keyboard: VIA "custom value" commands, 32 bytes:
//   [0x07 id_custom_set_value, CHANNEL, command, 0, payload...]
// The reply echoes the request with byte 3 set to a status, and any results in
// the payload bytes:
//   CMD_VERSION                → [PROTOCOL_VERSION, MATRIX_ROWS, MATRIX_COLS]
//   CMD_ENTER  [start, strike, release, pressure_step]   (ic_config_t, 1/60 mm)
//   CMD_PING                   keeps instrument mode alive (any command does)
//   CMD_EXIT
//   CMD_SET_PALETTE [index 1..3, r, g, b]
//   CMD_SET_ROW_COLORS [row, 15 palette indices (0 = unlit)]
//   CMD_GET_STATS              → [scans u32, max scan period µs u16, mean µs u16]
//                                since the previous CMD_GET_STATS (little-endian)
//
// Keyboard → host, unprompted while active, at most one report per ms:
//   [EVENT_REPORT, flags (bit 0: note events were lost), count, count × event]
//   event = [type, row, col, value lo, value hi]   (ic_event_t)

#include QMK_KEYBOARD_H
#include "analog_matrix.h"
#include "instrument_core.h"
#include "raw_hid.h"
#include "via.h"

#define CHANNEL 0x49
#define EVENT_REPORT 0x49
#define PROTOCOL_VERSION 1
#define WATCHDOG_MS 1000
#define REPORT_LEN 32
#define EVENTS_PER_REPORT ((REPORT_LEN - 3) / 5)

// The DWT cycle counter (enabled by ChibiOS at boot) runs at the core clock.
#define TICKS_PER_US (STM32_HCLK / 1000000)
#define SEND_INTERVAL_TICKS (1000 * TICKS_PER_US)

_Static_assert(MATRIX_ROWS == IC_ROWS && MATRIX_COLS == IC_COLS, "instrument_core sized for this matrix");
_Static_assert(STM32_HCLK % 1000000 == 0, "core clock not a whole number of MHz");

enum { CMD_VERSION, CMD_ENTER, CMD_PING, CMD_EXIT, CMD_SET_PALETTE, CMD_SET_ROW_COLORS, CMD_GET_STATS };
enum { STATUS_OK, STATUS_UNKNOWN_COMMAND, STATUS_BAD_ARGUMENT, STATUS_NOT_ACTIVE };

#define PALETTE_SIZE 4 // index 0 is unlit

extern const matrix_row_t analog_matrix_mask[];

static bool     active;
static ic_t     ic;
static uint32_t last_contact_ms; // timer_read32() at the last host command
static uint32_t last_send;       // cycle counter at the last event report
static uint8_t  palette[PALETTE_SIZE][3];
static uint8_t  colors[MATRIX_ROWS][MATRIX_COLS]; // palette indices

static struct {
    uint32_t scans;
    uint32_t max_period, total_period; // cycles
    uint32_t last_scan;
} stats;

static uint32_t now_ticks(void) {
    return DWT->CYCCNT;
}

static void read_travel(uint8_t travel[MATRIX_ROWS][MATRIX_COLS]) {
    for (uint8_t r = 0; r < MATRIX_ROWS; r++)
        for (uint8_t c = 0; c < MATRIX_COLS; c++) travel[r][c] = analog_matrix_get_travel(r, c);
}

static void enter(ic_config_t cfg) {
    uint8_t travel[MATRIX_ROWS][MATRIX_COLS];
    read_travel(travel);
    uint32_t now = now_ticks();
    ic_init(&ic, cfg, TICKS_PER_US * IC_DESCENT_UNIT_US, now, travel);
    // Release anything the host thinks is held; from here on keys don't type.
    clear_keyboard();
    last_send = now;
    active    = true;
}

static void put_u16(uint8_t *p, uint32_t v) {
    uint16_t x = v > 0xFFFF ? 0xFFFF : v;
    p[0]       = x & 0xFF;
    p[1]       = x >> 8;
}

static uint8_t handle(uint8_t command, uint8_t *p) {
    switch (command) {
        case CMD_VERSION:
            p[0] = PROTOCOL_VERSION;
            p[1] = MATRIX_ROWS;
            p[2] = MATRIX_COLS;
            return STATUS_OK;
        case CMD_ENTER: {
            ic_config_t cfg = {.start = p[0], .strike = p[1], .release = p[2], .pressure_step = p[3]};
            if (!ic_config_valid(cfg)) return STATUS_BAD_ARGUMENT;
            enter(cfg);
            return STATUS_OK;
        }
        case CMD_PING:
            return active ? STATUS_OK : STATUS_NOT_ACTIVE;
        case CMD_EXIT:
            active = false;
            return STATUS_OK;
        case CMD_SET_PALETTE:
            if (p[0] == 0 || p[0] >= PALETTE_SIZE) return STATUS_BAD_ARGUMENT;
            memcpy(palette[p[0]], &p[1], 3);
            return STATUS_OK;
        case CMD_SET_ROW_COLORS:
            if (p[0] >= MATRIX_ROWS) return STATUS_BAD_ARGUMENT;
            for (uint8_t c = 0; c < MATRIX_COLS; c++)
                if (p[1 + c] >= PALETTE_SIZE) return STATUS_BAD_ARGUMENT;
            memcpy(colors[p[0]], &p[1], MATRIX_COLS);
            return STATUS_OK;
        case CMD_GET_STATS:
            p[0] = stats.scans & 0xFF;
            p[1] = (stats.scans >> 8) & 0xFF;
            p[2] = (stats.scans >> 16) & 0xFF;
            p[3] = stats.scans >> 24;
            put_u16(&p[4], stats.max_period / TICKS_PER_US);
            put_u16(&p[6], stats.scans ? stats.total_period / stats.scans / TICKS_PER_US : 0);
            stats.scans = stats.max_period = stats.total_period = 0;
            return STATUS_OK;
        default:
            return STATUS_UNKNOWN_COMMAND;
    }
}

// Only called for channels QMK's core modules don't claim; VIA sends the
// (modified) buffer back as the reply afterwards.
void via_custom_value_command_kb(uint8_t *data, uint8_t length) {
    if (length != REPORT_LEN || data[0] != id_custom_set_value || data[1] != CHANNEL) {
        data[0] = id_unhandled;
        return;
    }
    last_contact_ms = timer_read32();
    data[3]         = handle(data[2], &data[4]);
}

static void send_events(void) {
    ic_event_t ev[EVENTS_PER_REPORT];
    bool       overflowed;
    uint8_t    n = ic_take_events(&ic, ev, EVENTS_PER_REPORT, &overflowed);
    if (n == 0 && !overflowed) return;

    uint8_t report[REPORT_LEN] = {EVENT_REPORT, overflowed, n};
    for (uint8_t i = 0; i < n; i++) {
        uint8_t *e = &report[3 + 5 * i];
        e[0]       = ev[i].type;
        e[1]       = ev[i].row;
        e[2]       = ev[i].col;
        e[3]       = ev[i].value & 0xFF;
        e[4]       = ev[i].value >> 8;
    }
    raw_hid_send(report, REPORT_LEN);
}

void matrix_scan_user(void) {
    uint32_t now    = now_ticks();
    uint32_t period = now - stats.last_scan;
    stats.last_scan = now;
    stats.scans++;
    stats.total_period += period;
    if (period > stats.max_period) stats.max_period = period;

    if (!active) return;
    if (timer_elapsed32(last_contact_ms) > WATCHDOG_MS) {
        active = false; // the host is gone: back to being a keyboard
        return;
    }

    uint8_t travel[MATRIX_ROWS][MATRIX_COLS];
    read_travel(travel);
    ic_scan(&ic, now, travel, analog_matrix_mask);

    if (now - last_send >= SEND_INTERVAL_TICKS) {
        send_events();
        last_send = now;
    }
}

bool pre_process_record_user(uint16_t keycode, keyrecord_t *record) {
    // Swallow keystrokes, but leave the encoder and the Mac/Win switch alone.
    return !(active && record->event.type == KEY_EVENT);
}

bool rgb_matrix_indicators_user(void) {
    if (!active) return true;
    uint8_t val = rgb_matrix_get_val();
    for (uint8_t i = 0; i < RGB_MATRIX_LED_COUNT; i++) rgb_matrix_set_color(i, 0, 0, 0);
    for (uint8_t r = 0; r < MATRIX_ROWS; r++) {
        for (uint8_t c = 0; c < MATRIX_COLS; c++) {
            uint8_t led = g_led_config.matrix_co[r][c];
            if (colors[r][c] == 0 || led == NO_LED) continue;
            const uint8_t *rgb = palette[colors[r][c]];
            rgb_matrix_set_color(led, rgb[0] * val / 255, rgb[1] * val / 255, rgb[2] * val / 255);
        }
    }
    return false; // skip Keychron's own indicators (caps lock etc.) too
}
