//! Standard MIDI File export.
//!
//! The byte format is the easy half. The half worth the care is what a *selection* of a continuous
//! MIDI stream even means, because three things straddle its edges and getting any of them wrong
//! produces a file that opens fine and plays wrong:
//!
//! * **Notes crossing the boundaries.** A note that started before the selection did not happen in
//!   it; a note that starts inside but is still held at the end has to be given one.
//! * **Controller state.** This is the one everybody forgets. A filter sweep that settled at CC 74
//!   = 12 before the selection began leaves no event inside it, so a naive export produces a clip
//!   with the cutoff wide open and the sustain pedal up. The state has to be *snapshotted* at the
//!   selection's start and emitted at tick zero.
//! * **Tempo.** A file with one tempo at the top plays everything after a change at the wrong
//!   speed, and unlike audio there is no waveform to notice it against.

use std::collections::BTreeMap;

use crate::grid::Selection;
use crate::tempo::{Meter, TempoMap};

/// Ticks per quarter note. 960 divides by 2, 3, 4, 5 and 8, so triplets and quintuplets land on
/// whole ticks rather than being rounded into a groove nobody played.
pub const DEFAULT_PPQ: u16 = 960;

#[derive(Clone, Copy, Debug)]
pub struct Note {
    pub start: u64,
    /// `None` for a note still held when the buffer was read.
    pub end: Option<u64>,
    pub channel: u8,
    pub key: u8,
    pub on_velocity: u8,
    pub off_velocity: u8,
}

#[derive(Clone, Copy, Debug)]
pub struct ControlChange {
    pub frame: u64,
    pub channel: u8,
    pub controller: u8,
    pub value: u8,
}

#[derive(Clone, Copy, Debug)]
pub struct PitchBend {
    pub frame: u64,
    pub channel: u8,
    /// 14-bit, centred at 8192.
    pub value: u16,
}

/// Everything captured on the MIDI lane, unclipped. The exporter does the clipping, because the
/// policy for doing so is the thing being specified.
#[derive(Clone, Copy, Debug, Default)]
pub struct Clip<'a> {
    pub notes: &'a [Note],
    pub controls: &'a [ControlChange],
    pub bends: &'a [PitchBend],
}

#[derive(Clone, Debug)]
pub struct SmfOptions {
    pub ppq: u16,
    /// Let a note that starts inside the selection ring past its end rather than being cut at it.
    ///
    /// Off by default. A rhythmic loop wants clean edges; a pad wants its tail. Neither answer is
    /// right for both, which is why this is a setting and not a decision.
    pub let_ring: bool,
    pub name: String,
}

impl Default for SmfOptions {
    fn default() -> SmfOptions {
        SmfOptions { ppq: DEFAULT_PPQ, let_ring: false, name: "Waveroll".into() }
    }
}

/// Order among events landing on the same tick.
///
/// Metadata first so a tempo applies to what follows it; the controller snapshot next so notes
/// sound with the state they were played under; note-offs before note-ons so a repeated key is
/// released before it is struck again rather than being cut short by its own predecessor.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Rank {
    Meta = 0,
    Control = 1,
    NoteOff = 2,
    NoteOn = 3,
}

struct Event {
    tick: u32,
    rank: Rank,
    bytes: Vec<u8>,
}

/// Writes a type 0 Standard MIDI File for `selection`.
pub fn write(selection: Selection, map: &TempoMap, clip: &Clip, options: &SmfOptions) -> Vec<u8> {
    let ppq = options.ppq.max(1);
    let (start, end) = (selection.start, selection.end);
    let origin = map.quarters_at(start);
    let tick = |frame: u64| -> u32 {
        let quarters = map.quarters_at(frame.max(start)) - origin;
        (quarters * f64::from(ppq)).round().max(0.0) as u32
    };
    let end_tick = tick(end);

    let mut events: Vec<Event> = Vec::new();
    let mut push = |tick: u32, rank: Rank, bytes: Vec<u8>| events.push(Event { tick, rank, bytes });

    // ---- metadata ----
    if !options.name.is_empty() {
        let mut bytes = vec![0xFF, 0x03];
        let name: Vec<u8> = options.name.bytes().take(127).collect();
        bytes.push(name.len() as u8);
        bytes.extend_from_slice(&name);
        push(0, Rank::Meta, bytes);
    }
    push(0, Rank::Meta, tempo_meta(map.bpm_at(start)));
    push(0, Rank::Meta, meter_meta(map.meter_at(start)));
    for (frame, bpm, meter) in map.changes_in(start, end) {
        push(tick(frame), Rank::Meta, tempo_meta(bpm));
        push(tick(frame), Rank::Meta, meter_meta(meter));
    }

    // ---- controller and bend state at the boundary ----
    let mut snapshot: BTreeMap<(u8, u8), u8> = BTreeMap::new();
    for control in clip.controls.iter().filter(|c| c.frame < start) {
        snapshot.insert((control.channel & 0x0F, control.controller & 0x7F), control.value & 0x7F);
    }
    for ((channel, controller), value) in snapshot {
        push(0, Rank::Control, vec![0xB0 | channel, controller, value]);
    }
    let mut bend_snapshot: BTreeMap<u8, u16> = BTreeMap::new();
    for bend in clip.bends.iter().filter(|b| b.frame < start) {
        bend_snapshot.insert(bend.channel & 0x0F, bend.value.min(16_383));
    }
    for (channel, value) in bend_snapshot {
        push(0, Rank::Control, vec![0xE0 | channel, (value & 0x7F) as u8, (value >> 7) as u8]);
    }

    // ---- events inside the selection ----
    for control in clip.controls.iter().filter(|c| c.frame >= start && c.frame < end) {
        push(
            tick(control.frame),
            Rank::Control,
            vec![0xB0 | (control.channel & 0x0F), control.controller & 0x7F, control.value & 0x7F],
        );
    }
    for bend in clip.bends.iter().filter(|b| b.frame >= start && b.frame < end) {
        let value = bend.value.min(16_383);
        push(
            tick(bend.frame),
            Rank::Control,
            vec![0xE0 | (bend.channel & 0x0F), (value & 0x7F) as u8, (value >> 7) as u8],
        );
    }

    // ---- notes ----
    //
    // Onset decides membership. A note that began before the selection is not in it however far it
    // rings, because including it would put an attack at tick zero that nobody played there.
    let mut last_tick = end_tick;
    for note in clip.notes.iter().filter(|n| n.start >= start && n.start < end) {
        let stop = match note.end {
            Some(stop) if options.let_ring => stop,
            Some(stop) => stop.min(end),
            // Still held when the buffer was read: it gets the boundary either way, since there is
            // no later information to give it.
            None => end,
        };
        let on = tick(note.start);
        // A note has to occupy at least one tick, or a host reading the pair back sees a release
        // that never had an attack.
        let off = tick(stop.max(note.start)).max(on + 1);
        let channel = note.channel & 0x0F;
        push(on, Rank::NoteOn, vec![0x90 | channel, note.key & 0x7F, note.on_velocity.clamp(1, 127)]);
        push(off, Rank::NoteOff, vec![0x80 | channel, note.key & 0x7F, note.off_velocity & 0x7F]);
        last_tick = last_tick.max(off);
    }

    // ---- serialise ----
    events.sort_by_key(|e| (e.tick, e.rank));

    let mut track = Vec::new();
    let mut previous = 0u32;
    for event in &events {
        write_varint(&mut track, event.tick - previous);
        track.extend_from_slice(&event.bytes);
        previous = event.tick;
    }
    write_varint(&mut track, last_tick.saturating_sub(previous));
    track.extend_from_slice(&[0xFF, 0x2F, 0x00]);

    let mut out = Vec::with_capacity(track.len() + 22);
    out.extend_from_slice(b"MThd");
    out.extend_from_slice(&6u32.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes()); // format 0: one track, everything merged
    out.extend_from_slice(&1u16.to_be_bytes()); // one track
    out.extend_from_slice(&ppq.to_be_bytes());
    out.extend_from_slice(b"MTrk");
    out.extend_from_slice(&(track.len() as u32).to_be_bytes());
    out.extend_from_slice(&track);
    out
}

fn tempo_meta(bpm: f64) -> Vec<u8> {
    // Microseconds per quarter note, in three bytes — which caps the slowest expressible tempo at
    // about 3.58 BPM and the clamp keeps a nonsense reading from wrapping into a fast one.
    let micros = (60_000_000.0 / bpm.max(0.001)).round().clamp(1.0, 0xFF_FFFF as f64) as u32;
    vec![0xFF, 0x51, 0x03, (micros >> 16) as u8, (micros >> 8) as u8, micros as u8]
}

fn meter_meta(meter: Meter) -> Vec<u8> {
    // The denominator is stored as its log base two. A meter whose denominator is not a power of
    // two cannot be expressed at all, so it is rounded to one rather than written as garbage.
    let dd = (meter.den as f64).log2().round().clamp(0.0, 7.0) as u8;
    // 24 MIDI clocks per metronome click, 8 thirty-second notes per quarter: the values every
    // sequencer writes, and the ones a reader assumes when it ignores the field.
    vec![0xFF, 0x58, 0x04, meter.num.clamp(1, 255) as u8, dd, 24, 8]
}

/// Variable-length quantity: seven bits per byte, high bit set on every byte but the last.
fn write_varint(out: &mut Vec<u8>, value: u32) {
    let mut buffer = [0u8; 4];
    let mut count = 0;
    let mut remaining = value & 0x0FFF_FFFF;
    loop {
        buffer[count] = (remaining & 0x7F) as u8;
        count += 1;
        remaining >>= 7;
        if remaining == 0 {
            break;
        }
    }
    for i in (0..count).rev() {
        out.push(buffer[i] | if i == 0 { 0 } else { 0x80 });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: u32 = 48_000;

    fn flat() -> TempoMap {
        TempoMap::new(SR, 120.0, Meter::FOUR_FOUR)
    }

    /// A reader written from the specification rather than from the writer, so a shared
    /// misunderstanding cannot pass both.
    struct Reader<'a> {
        bytes: &'a [u8],
        at: usize,
    }

    #[derive(Debug, PartialEq)]
    struct Read {
        tick: u32,
        bytes: Vec<u8>,
    }

    impl<'a> Reader<'a> {
        fn new(bytes: &'a [u8]) -> (u16, Reader<'a>) {
            assert_eq!(&bytes[0..4], b"MThd");
            assert_eq!(u32::from_be_bytes(bytes[4..8].try_into().unwrap()), 6);
            assert_eq!(u16::from_be_bytes(bytes[8..10].try_into().unwrap()), 0, "format 0");
            assert_eq!(u16::from_be_bytes(bytes[10..12].try_into().unwrap()), 1, "one track");
            let ppq = u16::from_be_bytes(bytes[12..14].try_into().unwrap());
            assert_eq!(&bytes[14..18], b"MTrk");
            let len = u32::from_be_bytes(bytes[18..22].try_into().unwrap()) as usize;
            assert_eq!(bytes.len(), 22 + len, "the track length must cover the rest of the file");
            (ppq, Reader { bytes: &bytes[22..], at: 0 })
        }

        fn varint(&mut self) -> u32 {
            let mut value = 0u32;
            loop {
                let byte = self.bytes[self.at];
                self.at += 1;
                value = (value << 7) | u32::from(byte & 0x7F);
                if byte & 0x80 == 0 {
                    return value;
                }
            }
        }

        fn all(&mut self) -> Vec<Read> {
            let mut out = Vec::new();
            let mut tick = 0;
            while self.at < self.bytes.len() {
                tick += self.varint();
                let status = self.bytes[self.at];
                let start = self.at;
                let length = match status {
                    0xFF => {
                        self.at += 2;
                        let len = self.varint() as usize;
                        self.at - start + len
                    }
                    s if s & 0xF0 == 0xC0 || s & 0xF0 == 0xD0 => 2,
                    s if s & 0x80 != 0 => 3,
                    _ => panic!("running status at {}: this writer must not emit it", self.at),
                };
                self.at = start + length;
                out.push(Read { tick, bytes: self.bytes[start..start + length].to_vec() });
            }
            out
        }
    }

    fn export(map: &TempoMap, clip: &Clip, sel: Selection, options: &SmfOptions) -> Vec<Read> {
        let bytes = write(sel, map, clip, options);
        let (ppq, mut reader) = Reader::new(&bytes);
        assert_eq!(ppq, options.ppq);
        reader.all()
    }

    #[test]
    fn variable_length_quantities_match_the_specification() {
        // The worked examples from the Standard MIDI File specification.
        for (value, expected) in [
            (0x0000_0000u32, vec![0x00]),
            (0x0000_0040, vec![0x40]),
            (0x0000_007F, vec![0x7F]),
            (0x0000_0080, vec![0x81, 0x00]),
            (0x0000_2000, vec![0xC0, 0x00]),
            (0x0000_3FFF, vec![0xFF, 0x7F]),
            (0x0000_4000, vec![0x81, 0x80, 0x00]),
            (0x0010_0000, vec![0xC0, 0x80, 0x00]),
            (0x001F_FFFF, vec![0xFF, 0xFF, 0x7F]),
            (0x0020_0000, vec![0x81, 0x80, 0x80, 0x00]),
            (0x0FFF_FFFF, vec![0xFF, 0xFF, 0xFF, 0x7F]),
        ] {
            let mut out = Vec::new();
            write_varint(&mut out, value);
            assert_eq!(out, expected, "{value:#x}");
        }
    }

    #[test]
    fn a_hundred_and_twenty_bpm_is_half_a_million_microseconds() {
        assert_eq!(tempo_meta(120.0), vec![0xFF, 0x51, 0x03, 0x07, 0xA1, 0x20]);
        assert_eq!(u32::from_be_bytes([0, 0x07, 0xA1, 0x20]), 500_000);
        // And the denominator really is a logarithm.
        assert_eq!(meter_meta(Meter::FOUR_FOUR), vec![0xFF, 0x58, 0x04, 4, 2, 24, 8]);
        assert_eq!(meter_meta(Meter::new(6, 8)), vec![0xFF, 0x58, 0x04, 6, 3, 24, 8]);
        assert_eq!(meter_meta(Meter::new(3, 4)), vec![0xFF, 0x58, 0x04, 3, 2, 24, 8]);
    }

    #[test]
    fn a_note_is_included_by_its_onset_and_cut_at_the_end() {
        let map = flat();
        let bar = |n: f64| map.frame_at_bars(n);
        let notes = [
            // Started before the selection: not in it, however far it rings.
            Note { start: bar(0.0), end: Some(bar(5.0)), channel: 0, key: 40, on_velocity: 100, off_velocity: 0 },
            // Squarely inside.
            Note { start: bar(4.5), end: Some(bar(4.75)), channel: 0, key: 60, on_velocity: 100, off_velocity: 0 },
            // Starts inside, rings past the end: cut.
            Note { start: bar(7.5), end: Some(bar(12.0)), channel: 0, key: 62, on_velocity: 90, off_velocity: 0 },
            // Still held when the buffer was read.
            Note { start: bar(6.0), end: None, channel: 0, key: 64, on_velocity: 80, off_velocity: 0 },
            // After the selection.
            Note { start: bar(9.0), end: Some(bar(9.5)), channel: 0, key: 67, on_velocity: 100, off_velocity: 0 },
        ];
        let clip = Clip { notes: &notes, ..Clip::default() };
        let sel = Selection { start: bar(4.0), end: bar(8.0) };
        let events = export(&map, &clip, sel, &SmfOptions::default());

        let keys: Vec<u8> = events
            .iter()
            .filter(|e| e.bytes[0] & 0xF0 == 0x90)
            .map(|e| e.bytes[1])
            .collect();
        assert_eq!(keys, vec![60, 64, 62], "only onsets inside the selection, in time order");

        // Four bars at 120 in 4/4 is sixteen quarters, so the file is 16 * 960 ticks long.
        let end_of_track = events.last().expect("a track ends");
        assert_eq!(end_of_track.bytes, vec![0xFF, 0x2F, 0x00]);
        assert_eq!(end_of_track.tick, 16 * 960);

        // Both the rung-past note and the held note stop exactly at the boundary.
        let offs: Vec<(u8, u32)> = events
            .iter()
            .filter(|e| e.bytes[0] & 0xF0 == 0x80)
            .map(|e| (e.bytes[1], e.tick))
            .collect();
        assert!(offs.contains(&(62, 16 * 960)), "the ringing note was not cut: {offs:?}");
        assert!(offs.contains(&(64, 16 * 960)), "the held note was not given an end: {offs:?}");
    }

    #[test]
    fn let_ring_keeps_the_tail() {
        let map = flat();
        let bar = |n: f64| map.frame_at_bars(n);
        let notes = [Note {
            start: bar(7.5), end: Some(bar(12.0)), channel: 0, key: 62, on_velocity: 90, off_velocity: 0,
        }];
        let clip = Clip { notes: &notes, ..Clip::default() };
        let sel = Selection { start: bar(4.0), end: bar(8.0) };
        let options = SmfOptions { let_ring: true, ..SmfOptions::default() };
        let events = export(&map, &clip, sel, &options);
        let off = events.iter().find(|e| e.bytes[0] & 0xF0 == 0x80).expect("a note off");
        assert_eq!(off.tick, 32 * 960, "eight bars past the selection start");
        // And the track has to be long enough to contain it.
        assert_eq!(events.last().unwrap().tick, 32 * 960);
    }

    #[test]
    fn controller_state_from_before_the_selection_is_emitted_at_tick_zero() {
        let map = flat();
        let bar = |n: f64| map.frame_at_bars(n);
        let controls = [
            ControlChange { frame: bar(0.5), channel: 0, controller: 74, value: 12 },
            ControlChange { frame: bar(1.0), channel: 0, controller: 64, value: 127 }, // sustain down
            ControlChange { frame: bar(2.0), channel: 0, controller: 74, value: 90 },  // superseded
            ControlChange { frame: bar(3.0), channel: 0, controller: 74, value: 33 },  // the one in force
            ControlChange { frame: bar(5.0), channel: 0, controller: 74, value: 100 }, // inside
        ];
        let bends = [
            PitchBend { frame: bar(1.5), channel: 0, value: 4096 },
            PitchBend { frame: bar(2.5), channel: 0, value: 10_000 },
        ];
        let notes = [Note {
            start: bar(4.5), end: Some(bar(5.5)), channel: 0, key: 60, on_velocity: 100, off_velocity: 0,
        }];
        let clip = Clip { notes: &notes, controls: &controls, bends: &bends };
        let sel = Selection { start: bar(4.0), end: bar(8.0) };
        let events = export(&map, &clip, sel, &SmfOptions::default());

        let at_zero: Vec<&Read> = events.iter().filter(|e| e.tick == 0).collect();
        assert!(
            at_zero.iter().any(|e| e.bytes == vec![0xB0, 74, 33]),
            "the cutoff in force at the boundary is missing: the clip would open wide"
        );
        assert!(
            at_zero.iter().any(|e| e.bytes == vec![0xB0, 64, 127]),
            "the sustain pedal was down and the clip has it up"
        );
        assert!(
            at_zero.iter().any(|e| e.bytes == vec![0xE0, 0x10, 0x4E]),
            "pitch bend was not restored: {at_zero:?}"
        );
        assert!(
            !at_zero.iter().any(|e| e.bytes == vec![0xB0, 74, 90]),
            "only the latest value per controller belongs in the snapshot"
        );

        // The snapshot has to precede the first note, or it changes the sound after the attack.
        let first_note = events.iter().position(|e| e.bytes[0] & 0xF0 == 0x90).unwrap();
        let last_snapshot = events.iter().rposition(|e| e.tick == 0 && e.bytes[0] == 0xB0).unwrap();
        assert!(last_snapshot < first_note);
    }

    #[test]
    fn a_tempo_change_inside_the_selection_is_carried_into_the_file() {
        let mut map = TempoMap::new(SR, 120.0, Meter::FOUR_FOUR);
        let change = map.frame_at_bars(6.0);
        map.push(change, 90.0, Meter::new(3, 4));
        let sel = Selection { start: map.frame_at_bars(4.0), end: map.frame_at_bars(8.0) };
        let events = export(&map, &Clip::default(), sel, &SmfOptions::default());

        let tempos: Vec<(u32, u32)> = events
            .iter()
            .filter(|e| e.bytes.starts_with(&[0xFF, 0x51]))
            .map(|e| {
                (e.tick, u32::from_be_bytes([0, e.bytes[3], e.bytes[4], e.bytes[5]]))
            })
            .collect();
        assert_eq!(
            tempos,
            vec![(0, 500_000), (2 * 4 * 960, 666_667)],
            "expected 120 at the top and 90 two bars in"
        );
        let meters: Vec<(u32, u8, u8)> = events
            .iter()
            .filter(|e| e.bytes.starts_with(&[0xFF, 0x58]))
            .map(|e| (e.tick, e.bytes[3], e.bytes[4]))
            .collect();
        assert_eq!(meters, vec![(0, 4, 2), (2 * 4 * 960, 3, 2)]);
    }

    #[test]
    fn a_note_never_has_zero_length() {
        let map = flat();
        let bar = |n: f64| map.frame_at_bars(n);
        // A note lasting a handful of samples: far under one tick at 960 PPQ.
        let notes = [Note {
            start: bar(4.0) + 10, end: Some(bar(4.0) + 12), channel: 3, key: 60,
            on_velocity: 0, off_velocity: 0,
        }];
        let clip = Clip { notes: &notes, ..Clip::default() };
        let sel = Selection { start: bar(4.0), end: bar(8.0) };
        let events = export(&map, &clip, sel, &SmfOptions::default());
        let on = events.iter().find(|e| e.bytes[0] & 0xF0 == 0x90).expect("note on");
        let off = events.iter().find(|e| e.bytes[0] & 0xF0 == 0x80).expect("note off");
        assert!(off.tick > on.tick, "a note off must come after its note on");
        assert_eq!(on.bytes[0] & 0x0F, 3, "channel is preserved");
        assert_eq!(on.bytes[2], 1, "velocity zero would be read back as a note off");
    }

    #[test]
    fn an_empty_selection_still_produces_a_valid_file() {
        let map = flat();
        let sel = Selection { start: map.frame_at_bars(4.0), end: map.frame_at_bars(8.0) };
        let events = export(&map, &Clip::default(), sel, &SmfOptions::default());
        assert_eq!(events.last().unwrap().bytes, vec![0xFF, 0x2F, 0x00]);
        assert_eq!(events.last().unwrap().tick, 16 * 960, "the clip keeps its length");
    }
}
