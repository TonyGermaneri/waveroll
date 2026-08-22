/*
 * The C ABI between the JUCE shell and the Rust core.
 *
 * The C++ side owns plugin formats, a window and the file drag. Everything else -- the ring, the
 * clock, the tempo map, snapping, and writing the file -- is on the other side of this header and
 * is tested without a host. Nothing here has logic of its own; if it grew some, that would be
 * logic living where it cannot be tested.
 */
#pragma once
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct WrCore WrCore;

/** What the host reports, once per block. */
typedef struct
{
    bool     playing;
    /** True during an offline bounce. Capture refuses these, or exporting a track would quietly
        replace the take with the export. */
    bool     offline;
    double   bpm;
    uint32_t numerator;
    uint32_t denominator;
} WrTransport;

/** What the editor needs to draw its chrome. */
typedef struct
{
    double   bpm;
    bool     playing;
    bool     held;
    uint64_t captured;
    uint64_t lap;
    double   head;            /**< write head as a fraction of the editor width */
    double   window_bars;
    double   unit_bars;
    double   zoom;            /**< 1.0 is fit to width */
    bool     has_selection;
    double   selection_bars;
    double   selection_from;  /**< canvas fractions; outside 0..1 when off screen */
    double   selection_to;
    uint32_t selection_state; /**< 0 empty, 1 pending, 2 overwritten, 3 ready */
    uint32_t markers;
} WrStatus;

void*    wr_create (uint32_t sample_rate, uint32_t channels, uint32_t capacity_log2,
                    uint32_t max_block);
void     wr_destroy (void* core);

/** Real-time safe. Returns frames taken; zero when stopped or rendering offline. */
uint32_t wr_capture (void* core, const float* const* channels, uint32_t frames,
                     const WrTransport* transport);

/** Call after wr_capture for the same block. Does nothing when that block was refused. */
void     wr_capture_midi (void* core, uint32_t offset_in_block,
                          uint8_t status, uint8_t data1, uint8_t data2);

/** Renders the selection's MIDI as a Standard MIDI File; 0 when the lane is empty there. */
size_t         wr_stage_midi (void* core, bool let_ring);
const uint8_t* wr_staged_midi_bytes (void* core);

void     wr_set_width (void* core, uint32_t width);
void     wr_set_window_bars (void* core, double bars);
void     wr_set_unit (void* core, double bars);   /**< 0 means auto */
/** Step the quantise setting along the ladder; returns the new unit in bars, 0 for auto. */
double   wr_cycle_unit (void* core, int32_t direction);
/** Step the window length; returns the new length in bars. */
double   wr_cycle_window (void* core, int32_t direction);
void     wr_zoom (void* core, double factor, double anchor);
void     wr_home (void* core);

void     wr_click (void* core, double fraction);
void     wr_drag (void* core, double from, double to);
void     wr_select_percent (void* core, uint32_t tenths);
void     wr_clear_selection (void* core);
void     wr_hold (void* core, bool on);
void     wr_mark (void* core);
bool     wr_select_from_marker (void* core);
void     wr_set_downbeat_now (void* core);

/** Renders the selection to WAV internally; returns its length, or 0 when refused. */
size_t         wr_stage (void* core, uint64_t time_reference);
/** Valid until the next wr_stage. */
const uint8_t* wr_staged_bytes (void* core);
double         wr_selection_bars (void* core);

void     wr_status (void* core, WrStatus* out);

/* The picture. `native_view` is an NSView* that must outlive the returned handle; a null return
   means the GPU could not be opened, and the plugin should carry on capturing without a display
   rather than take the host down with it. */
void*    wr_view_open (void* core, void* native_view, uint32_t width, uint32_t height, double scale);
void     wr_view_resize (void* view, uint32_t width, uint32_t height, double scale);
void     wr_view_draw (void* core, void* view);
void     wr_view_close (void* view);
size_t   wr_view_describe (void* view, uint8_t* out, size_t cap);

#ifdef __cplusplus
}
#endif
