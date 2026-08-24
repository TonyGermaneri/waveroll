//! The C ABI, exercised the way the plugin exercises it.
//!
//! This is the surface the whole native build stands on, every function of it is `unsafe`, and its
//! callers are C++ compilers that will not check anything. The two questions worth asking are
//! whether it does the right thing when driven correctly, and whether it does something survivable
//! when driven wrongly — because "driven wrongly" here means a host crashing.

use std::ffi::c_void;

use waveroll_ffi::*;

const SR: u32 = 48_000;
const BLOCK: u32 = 512;

struct Core(*mut c_void);

impl Core {
    fn new(channels: u32) -> Core {
        let core = wr_create(SR, channels, 20, BLOCK);
        assert!(!core.is_null(), "the core should allocate");
        wr_set_width(core, 1000);
        Core(core)
    }

    fn status(&self) -> WrStatus {
        let mut out = WrStatus::default();
        wr_status(self.0, &mut out);
        out
    }

    /// Captures `blocks` blocks of a constant value, rolling at 120 BPM.
    fn capture(&self, blocks: usize, value: f32, playing: bool, offline: bool) {
        let transport =
            WrTransport { playing, offline, bpm: 120.0, numerator: 4, denominator: 4,
                          has_position: false, position: 0.0 };
        let samples = vec![value; BLOCK as usize];
        let pointers = [samples.as_ptr(), samples.as_ptr()];
        for _ in 0..blocks {
            wr_capture(self.0, pointers.as_ptr(), 2, BLOCK, &transport);
        }
    }

    /// Captures `blocks` blocks rolling at 120 BPM, threading the song position along with them.
    /// Returns where the playhead ends up, in quarter notes.
    fn capture_from(&self, blocks: usize, value: f32, quarters: f64) -> f64 {
        let samples = vec![value; BLOCK as usize];
        let pointers = [samples.as_ptr(), samples.as_ptr()];
        let mut at = quarters;
        let per_block = f64::from(BLOCK) * 120.0 / (60.0 * f64::from(SR));
        for _ in 0..blocks {
            let transport = WrTransport {
                playing: true,
                offline: false,
                bpm: 120.0,
                numerator: 4,
                denominator: 4,
                has_position: true,
                position: at,
            };
            wr_capture(self.0, pointers.as_ptr(), 2, BLOCK, &transport);
            at += per_block;
        }
        at
    }

    /// Blocks for one bar at 120 BPM: two seconds.
    fn blocks_per_bar() -> usize {
        (SR as usize * 2) / BLOCK as usize
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        wr_destroy(self.0);
    }
}

// ---------------------------------------------------------------------------------------
// Driven wrongly
// ---------------------------------------------------------------------------------------

/// Every entry point, with a null core, in one test.
///
/// A host that unloads a plugin mid-callback, or a C++ path that forgot a guard, must get a call
/// that does nothing rather than a segmentation fault inside somebody's session. This is the
/// cheapest test in the project and covers the failure with the worst consequences.
#[test]
fn every_entry_point_tolerates_a_null_core() {
    let null = std::ptr::null_mut();
    let transport = WrTransport {
        playing: true,
        offline: false,
        bpm: 120.0,
        numerator: 4,
        denominator: 4,
        has_position: false,
        position: 0.0,
    };
    let samples = [0.0f32; 64];
    let pointers = [samples.as_ptr(), samples.as_ptr()];

    wr_destroy(null);
    assert_eq!(wr_capture(null, pointers.as_ptr(), 2, 64, &transport), 0);
    wr_capture_midi(null, 0, 0x90, 60, 100);
    assert_eq!(wr_cycle_unit(null, 1), 0.0);
    assert_eq!(wr_cycle_window(null, 1), 0.0);
    wr_zoom(null, 2.0, 0.5);
    wr_home(null);
    wr_set_width(null, 100);
    wr_set_window_bars(null, 16.0);
    wr_set_unit(null, 1.0);
    wr_click(null, 0.5);
    wr_drag(null, 0.1, 0.9);
    wr_select_percent(null, 3);
    wr_clear_selection(null);
    wr_hold(null, true);
    wr_mark(null);
    assert!(!wr_select_from_marker(null));
    wr_set_downbeat_now(null);
    assert_eq!(wr_stage(null, 0), 0);
    assert!(wr_staged_bytes(null).is_null());
    assert_eq!(wr_stage_midi(null, false), 0);
    assert!(wr_staged_midi_bytes(null).is_null());
    assert_eq!(wr_selection_bars(null), 0.0);
    wr_status(null, std::ptr::null_mut());

    // The view half, which also takes its own handle.
    assert!(wr_view_open(null, null, 100, 100, 1.0).is_null());
    wr_view_resize(null, 100, 100, 1.0);
    wr_view_draw(null, null);
    wr_view_close(null);
    assert_eq!(wr_view_describe(null, std::ptr::null_mut(), 0), 0);
    assert_eq!(wr_view_last_error(null, std::ptr::null_mut(), 0), 0);
}

#[test]
fn a_null_status_pointer_is_not_written_through() {
    let core = Core::new(2);
    wr_status(core.0, std::ptr::null_mut());
}

#[test]
fn a_null_transport_captures_nothing() {
    let core = Core::new(2);
    let samples = [0.0f32; 64];
    let pointers = [samples.as_ptr(), samples.as_ptr()];
    assert_eq!(wr_capture(core.0, pointers.as_ptr(), 2, 64, std::ptr::null()), 0);
    assert_eq!(core.status().captured, 0);
}

#[test]
fn absurd_arguments_are_clamped_rather_than_believed() {
    let core = Core::new(2);
    // Values a MIDI binding or a corrupted preset could produce.
    wr_set_window_bars(core.0, -5.0);
    assert!(core.status().window_bars >= 1.0, "a window may not be negative");
    wr_set_window_bars(core.0, 1.0e9);
    assert!(core.status().window_bars <= 512.0, "nor unbounded");
    wr_zoom(core.0, 0.0, 0.5);
    wr_zoom(core.0, f64::INFINITY, 2.0);
    wr_zoom(core.0, f64::NAN, -1.0);
    let zoom = core.status().zoom;
    assert!(zoom.is_finite() && zoom >= 1.0, "zoom escaped: {zoom}");
    wr_select_percent(core.0, 99_999);
    wr_click(core.0, f64::NAN);
    wr_drag(core.0, -10.0, 10.0);
}

// ---------------------------------------------------------------------------------------
// Driven correctly
// ---------------------------------------------------------------------------------------

#[test]
fn capture_follows_the_transport() {
    let core = Core::new(2);
    core.capture(20, 0.5, false, false);
    assert_eq!(core.status().captured, 0, "stopped means nothing is recorded");

    core.capture(20, 0.5, true, false);
    assert_eq!(core.status().captured, 20 * u64::from(BLOCK));
    assert!(core.status().playing);

    // The bug that would silently replace a take with a bounce.
    core.capture(1000, 0.5, true, true);
    assert_eq!(
        core.status().captured,
        20 * u64::from(BLOCK),
        "an offline render must contribute nothing"
    );
}

#[test]
fn a_restart_elsewhere_in_the_song_puts_the_grid_back_on_the_music() {
    // The whole path, as the plugin drives it. Roll for a while, stop — which in almost every DAW
    // sends the playhead back to the top — and play again from somewhere else entirely. What the
    // editor draws has to go on landing where the music is.
    let core = Core::new(2);
    core.capture_from(Core::blocks_per_bar() * 5 + 7, 0.5, 0.0);
    core.capture(20, 0.0, false, false); // stopped

    let restart = 37.3; // bar ten and a third, nothing to do with where it stopped
    let blocks = Core::blocks_per_bar() * 4;
    let ended = core.capture_from(blocks, 0.5, restart);

    let status = core.status();
    // `head` is a fraction of a 16-bar window, so this is the head in absolute bars within the lap.
    let phase = (status.head * status.window_bars).rem_euclid(1.0);
    // The head is one block behind where the song got to: the last block's position is reported
    // at its start, and it has been captured since.
    let expected = (ended / 4.0).rem_euclid(1.0);
    let off = (phase - expected).rem_euclid(1.0);
    assert!(
        off.min(1.0 - off) < 0.005,
        "the take sits at {phase} of a bar and the song is at {expected}"
    );
}

#[test]
fn a_selection_across_a_restart_is_still_the_length_it_says() {
    // The reason the splice is frames rather than a hole in bar space: a four-bar selection has to
    // hold four bars of audio whatever happened in the middle of it, or the loop does not loop.
    let core = Core::new(2);
    core.capture_from(Core::blocks_per_bar() * 3 + 11, 0.5, 0.0);
    core.capture(20, 0.0, false, false);
    core.capture_from(Core::blocks_per_bar() * 8, 0.5, 37.3);

    wr_set_unit(core.0, 1.0);
    wr_select_percent(core.0, 5); // half of sixteen bars
    let status = core.status();
    assert_eq!(status.selection_state, 3, "it should be ready to stage");
    assert!((status.selection_bars - 8.0).abs() < 1e-6, "got {}", status.selection_bars);

    let length = wr_stage(core.0, 0);
    let bytes = unsafe { std::slice::from_raw_parts(wr_staged_bytes(core.0), length) };
    let data = find_chunk(bytes, b"data").expect("a data chunk");
    // Eight bars of 4/4 at 120 BPM is sixteen seconds; stereo f32 is eight bytes a frame.
    assert_eq!(
        data.len(),
        16 * SR as usize * 8,
        "a splice inside the selection changed how long it is"
    );
}

#[test]
fn a_mono_host_buffer_still_fills_a_stereo_core() {
    // A host may hand over fewer channels than the core was configured for. The ring already
    // duplicates its last plane for exactly this case; the boundary has to let it.
    let core = Core::new(2);
    let samples = vec![0.25f32; BLOCK as usize];
    let pointers = [samples.as_ptr(), std::ptr::null()];
    let transport =
        WrTransport { playing: true, offline: false, bpm: 120.0, numerator: 4, denominator: 4,
                      has_position: false, position: 0.0 };
    let taken = wr_capture(core.0, pointers.as_ptr(), 2, BLOCK, &transport);
    assert_eq!(taken, BLOCK, "a mono buffer must not silently capture nothing");
    assert_eq!(core.status().captured, u64::from(BLOCK));
}

#[test]
fn the_number_row_selects_whole_cells_and_stages_them() {
    let core = Core::new(2);
    // Two laps of a 16-bar window, so there is plenty behind the head.
    core.capture(Core::blocks_per_bar() * 40, 0.5, true, false);

    wr_set_unit(core.0, 1.0);
    wr_select_percent(core.0, 2); // the last 20% of sixteen bars
    let status = core.status();
    assert!(status.has_selection);
    assert_eq!(status.selection_state, 3, "it should be ready to stage");
    assert!(
        (status.selection_bars - 3.0).abs() < 1e-6,
        "20% of sixteen bars is 3.2, which rounds to three whole bars, got {}",
        status.selection_bars
    );

    let length = wr_stage(core.0, 0);
    assert!(length > 44, "a WAV is more than a header");
    let bytes = unsafe { std::slice::from_raw_parts(wr_staged_bytes(core.0), length) };
    assert_eq!(&bytes[0..4], b"RIFF");
    assert_eq!(&bytes[8..12], b"WAVE");

    // Three bars of 4/4 at 120 BPM is six seconds; stereo f32 is eight bytes a frame.
    let data = find_chunk(bytes, b"data").expect("a data chunk");
    assert_eq!(data.len(), 6 * SR as usize * 8, "the audio is not the length the selection was");
}

#[test]
fn staging_refuses_a_selection_the_writer_has_overtaken() {
    let core = Core::new(1);
    core.capture(Core::blocks_per_bar() * 20, 0.5, true, false);
    wr_set_unit(core.0, 1.0);
    wr_select_percent(core.0, 5);
    assert!(wr_stage(core.0, 0) > 0, "it stages while it is still there");

    // Hold, so the selection stops being trimmed, then run the writer far past it. A partly
    // overwritten read is a seam of old and new audio that looks entirely plausible.
    wr_hold(core.0, true);
    core.capture(Core::blocks_per_bar() * 200, 0.9, true, false);
    assert_eq!(core.status().selection_state, 2, "it should report itself overwritten");
    assert_eq!(wr_stage(core.0, 0), 0, "and refuse rather than hand over a seam");
}

#[test]
fn a_selection_erodes_as_the_head_reaches_it_rather_than_vanishing() {
    let core = Core::new(1);
    core.capture(Core::blocks_per_bar() * 20, 0.5, true, false);
    wr_set_unit(core.0, 1.0);
    wr_select_percent(core.0, 10); // the whole window
    let before = core.status().selection_bars;
    // Not necessarily sixteen. The head is almost never on a bar line, and both ends snap to whole
    // cells, so "everything" is the most whole bars that fit inside the window — which is fifteen
    // when the window's old edge falls partway through a bar. That is the price of a selection
    // that always loops, and it is paid here rather than hidden.
    assert!(
        (15.0..=16.0).contains(&before),
        "expected the most whole bars that fit in sixteen, got {before}"
    );

    // Four more bars: the oldest four should have been given up, not the whole selection.
    core.capture(Core::blocks_per_bar() * 4, 0.5, true, false);
    let after = core.status();
    assert!(after.has_selection, "it must not vanish because the head touched it");
    assert!(
        (after.selection_bars - (before - 4.0)).abs() < 1e-6,
        "expected {} bars left after giving up four, got {}",
        before - 4.0,
        after.selection_bars
    );
}

#[test]
fn the_grid_ladder_walks_and_stops_at_both_ends() {
    let core = Core::new(1);
    // Down from anywhere reaches auto, which reports as zero, and stays there.
    for _ in 0..12 {
        wr_cycle_unit(core.0, -1);
    }
    assert_eq!(wr_cycle_unit(core.0, -1), 0.0, "auto is the bottom");
    // Up from auto reaches the finest rung and climbs to the coarsest.
    assert!((wr_cycle_unit(core.0, 1) - 1.0 / 32.0).abs() < 1e-9);
    for _ in 0..12 {
        wr_cycle_unit(core.0, 1);
    }
    assert_eq!(wr_cycle_unit(core.0, 1), 4.0, "four bars is the top");
}

#[test]
fn midi_is_captured_on_the_same_axis_as_the_audio_and_exports() {
    let core = Core::new(1);
    let transport =
        WrTransport { playing: true, offline: false, bpm: 120.0, numerator: 4, denominator: 4,
                      has_position: false, position: 0.0 };
    let samples = vec![0.1f32; BLOCK as usize];
    let pointers = [samples.as_ptr(), samples.as_ptr()];

    // A note every other block, over several bars.
    let blocks = Core::blocks_per_bar() * 8;
    for i in 0..blocks {
        wr_capture(core.0, pointers.as_ptr(), 1, BLOCK, &transport);
        if i % 8 == 0 {
            wr_capture_midi(core.0, 0, 0x90, 60, 100);
        } else if i % 8 == 4 {
            wr_capture_midi(core.0, 0, 0x80, 60, 0);
        }
    }

    wr_set_unit(core.0, 1.0);
    wr_select_percent(core.0, 3);
    let length = wr_stage_midi(core.0, false);
    assert!(length > 22, "an SMF is more than a header");
    let bytes = unsafe { std::slice::from_raw_parts(wr_staged_midi_bytes(core.0), length) };
    assert_eq!(&bytes[0..4], b"MThd");
    assert_eq!(&bytes[14..18], b"MTrk");
    assert_eq!(
        u32::from_be_bytes(bytes[18..22].try_into().expect("four bytes")) as usize,
        length - 22,
        "the track length must cover the rest of the file"
    );
}

#[test]
fn midi_arriving_for_a_refused_block_is_refused_too() {
    // Otherwise a clip contains notes from a passage whose audio was never recorded.
    let core = Core::new(1);
    let stopped =
        WrTransport { playing: false, offline: false, bpm: 120.0, numerator: 4, denominator: 4,
                      has_position: false, position: 0.0 };
    let samples = vec![0.0f32; BLOCK as usize];
    let pointers = [samples.as_ptr(), samples.as_ptr()];
    wr_capture(core.0, pointers.as_ptr(), 1, BLOCK, &stopped);
    wr_capture_midi(core.0, 0, 0x90, 60, 100);

    core.capture(Core::blocks_per_bar() * 8, 0.1, true, false);
    wr_set_unit(core.0, 1.0);
    wr_select_percent(core.0, 10);
    assert_eq!(wr_stage_midi(core.0, false), 0, "the lane should be empty over this selection");
}

#[test]
fn hold_freezes_the_head_the_editor_reads() {
    let core = Core::new(1);
    core.capture(Core::blocks_per_bar() * 4, 0.5, true, false);
    wr_hold(core.0, true);
    let held = core.status();
    assert!(held.held);

    core.capture(Core::blocks_per_bar() * 4, 0.5, true, false);
    let after = core.status();
    assert_eq!(after.head, held.head, "the picture must stop moving");
    assert!(after.captured > held.captured, "while capture carries on underneath");

    wr_hold(core.0, false);
    assert!(core.status().head != held.head, "and resumes on release");
}

#[test]
fn markers_select_backwards_from_the_head() {
    let core = Core::new(1);
    core.capture(Core::blocks_per_bar() * 4, 0.5, true, false);
    assert!(!wr_select_from_marker(core.0), "there is no marker yet");

    wr_mark(core.0);
    assert_eq!(core.status().markers, 1);
    core.capture(Core::blocks_per_bar() * 3, 0.5, true, false);

    wr_set_unit(core.0, 1.0);
    assert!(wr_select_from_marker(core.0), "three whole bars have passed since the mark");
    let status = core.status();
    assert!(
        (status.selection_bars - 3.0).abs() < 1e-6,
        "expected the three bars since the mark, got {}",
        status.selection_bars
    );
}

/// Walks a RIFF file for one chunk. Deliberately naive: if the writer's padding is wrong this
/// desynchronises and the caller fails, which is the point.
fn find_chunk<'a>(wav: &'a [u8], id: &[u8; 4]) -> Option<&'a [u8]> {
    let mut at = 12;
    while at + 8 <= wav.len() {
        let size = u32::from_le_bytes(wav[at + 4..at + 8].try_into().ok()?) as usize;
        if &wav[at..at + 4] == id {
            return wav.get(at + 8..at + 8 + size);
        }
        at += 8 + size + (size % 2);
    }
    None
}
