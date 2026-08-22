//! The MIDI lane: a ring of raw events, and the reading of them into notes.
//!
//! The split here matters. The audio thread does the least possible work — copy three bytes and a
//! frame index into a fixed ring — and every interpretation happens later, on whatever thread is
//! asking. Pairing note-ons with note-offs, tracking which controller was where, deciding what a
//! note straddling a selection edge means: none of that belongs on a thread that must not
//! allocate and must not think.
//!
//! Events are stamped in **capture frames**, the same axis as the audio, so a MIDI selection and
//! an audio selection of the same bars cover the same moment. Stamping in milliseconds would put
//! the two on different clocks and they would drift apart over a session.

/// One captured MIDI message. Three bytes covers everything with a channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Event {
    pub frame: u64,
    pub status: u8,
    pub data1: u8,
    pub data2: u8,
}

impl Event {
    pub fn kind(&self) -> u8 {
        self.status & 0xF0
    }
    pub fn channel(&self) -> u8 {
        self.status & 0x0F
    }

    /// A note-on with velocity zero is a note-off, and most keyboards send it that way. Treating
    /// the two differently is the classic way to end up with notes that never stop.
    pub fn is_note_on(&self) -> bool {
        self.kind() == 0x90 && self.data2 > 0
    }
    pub fn is_note_off(&self) -> bool {
        self.kind() == 0x80 || (self.kind() == 0x90 && self.data2 == 0)
    }
}

/// A paired note. `end` is `None` for one still held when the buffer was read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Note {
    pub start: u64,
    pub end: Option<u64>,
    pub channel: u8,
    pub key: u8,
    pub on_velocity: u8,
    pub off_velocity: u8,
}

/// Controller state, for the snapshot emitted at the start of an exported clip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Control {
    pub channel: u8,
    pub controller: u8,
    pub value: u8,
}

/// A fixed, overwriting ring of MIDI events.
///
/// Overwriting rather than blocking, for the same reason the audio ring is: the producer is on the
/// audio thread and must never wait. Old events falling off the back is exactly right — they are
/// out of the window and unselectable anyway.
#[derive(Debug)]
pub struct MidiRing {
    events: Box<[Event]>,
    mask: usize,
    write: u64,
}

impl MidiRing {
    /// # Panics
    /// If `capacity` is not a power of two.
    pub fn new(capacity: usize) -> MidiRing {
        assert!(capacity.is_power_of_two(), "capacity must be a power of two, got {capacity}");
        MidiRing {
            events: vec![Event::default(); capacity].into_boxed_slice(),
            mask: capacity - 1,
            write: 0,
        }
    }

    pub fn capacity(&self) -> usize {
        self.events.len()
    }
    pub fn written(&self) -> u64 {
        self.write
    }

    /// Producer entry point. Real-time safe: one store, no allocation, no branching on a consumer.
    ///
    /// Channel-voice messages only. Clock and transport arrive through `ClockPll`, and system
    /// exclusive has no place in a captured performance.
    ///
    /// **Frames must not go backwards.** Everything that reads this ring relies on it, and the
    /// audio thread satisfies it for free because capture frames only ever increase. Checked in
    /// debug rather than sorted at read time: a caller breaking it has a bug, and quietly coping
    /// would hide it behind a slower reader.
    pub fn push(&mut self, frame: u64, status: u8, data1: u8, data2: u8) {
        debug_assert!(
            self.write == 0 || frame >= self.events[((self.write - 1) as usize) & self.mask].frame,
            "MIDI events must be pushed in frame order"
        );
        if !(0x80..0xF0).contains(&status) {
            return;
        }
        let at = (self.write as usize) & self.mask;
        self.events[at] = Event { frame, status, data1, data2 };
        self.write += 1;
    }

    /// Every event still in the ring, oldest first.
    pub fn iter(&self) -> impl Iterator<Item = Event> + '_ {
        let held = (self.write as usize).min(self.events.len());
        let start = self.write - held as u64;
        (0..held).map(move |i| self.events[((start + i as u64) as usize) & self.mask])
    }

    /// Notes whose **onset** falls in `[start, end)`.
    ///
    /// Onset decides membership. A note that began before the selection is not in it however far
    /// it rings — including it would put an attack at tick zero that nobody played there.
    ///
    /// A note still sounding at `end` is truncated there unless `let_ring`, and one never released
    /// at all gets `end: None` for the caller to place. A rhythmic loop wants clean edges and a pad
    /// wants its tail; neither answer is right for both, which is why it is an argument.
    pub fn notes_in(&self, start: u64, end: u64, let_ring: bool) -> Vec<Note> {
        // Keyed by (channel, key): the same key can be re-struck before its release arrives, and a
        // map keyed on key alone would pair the second onset with the first release.
        let mut open: Vec<(u8, u8, usize)> = Vec::new();
        let mut notes: Vec<Note> = Vec::new();

        // The whole ring is scanned, never stopping at `end`. A release arriving after the
        // boundary is what distinguishes a note that was let go late from one never released at
        // all, and those get different endings -- so the search cannot stop before finding out.
        for event in self.iter() {
            if event.is_note_on() {
                if event.frame >= start && event.frame < end {
                    notes.push(Note {
                        start: event.frame,
                        end: None,
                        channel: event.channel(),
                        key: event.data1,
                        on_velocity: event.data2,
                        off_velocity: 0,
                    });
                    open.push((event.channel(), event.data1, notes.len() - 1));
                } else {
                    // Onset outside the selection: remembered as open only so its release does not
                    // close somebody else's note.
                    open.push((event.channel(), event.data1, usize::MAX));
                }
            } else if event.is_note_off()
                && let Some(position) =
                    open.iter().rposition(|(c, k, _)| *c == event.channel() && *k == event.data1)
            {
                let (_, _, index) = open.remove(position);
                if let Some(note) = notes.get_mut(index) {
                    note.end = Some(event.frame);
                    note.off_velocity = event.data2;
                }
            }
        }

        for note in &mut notes {
            if let Some(stop) = note.end
                && !let_ring
                && stop > end
            {
                note.end = Some(end);
            }
        }
        notes
    }

    /// The controller values in force just before `at`, one per (channel, controller).
    ///
    /// The thing everybody forgets. A filter sweep that settled before the selection began leaves
    /// no event inside it, so a naive export produces a clip with the cutoff wide open and the
    /// sustain pedal up. Emitting these at tick zero is what makes a dropped clip sound like what
    /// was played.
    pub fn controls_before(&self, at: u64) -> Vec<Control> {
        let mut latest: Vec<Control> = Vec::new();
        for event in self.iter() {
            if event.frame >= at {
                break;
            }
            if event.kind() != 0xB0 {
                continue;
            }
            let control =
                Control { channel: event.channel(), controller: event.data1, value: event.data2 };
            match latest
                .iter_mut()
                .find(|c| c.channel == control.channel && c.controller == control.controller)
            {
                Some(existing) => *existing = control,
                None => latest.push(control),
            }
        }
        latest
    }

    /// Pitch bend in force just before `at`, per channel, as a 14-bit value.
    pub fn bends_before(&self, at: u64) -> Vec<(u8, u16)> {
        let mut latest: Vec<(u8, u16)> = Vec::new();
        for event in self.iter() {
            if event.frame >= at {
                break;
            }
            if event.kind() != 0xE0 {
                continue;
            }
            let value = u16::from(event.data1) | (u16::from(event.data2) << 7);
            match latest.iter_mut().find(|(c, _)| *c == event.channel()) {
                Some(existing) => existing.1 = value,
                None => latest.push((event.channel(), value)),
            }
        }
        latest
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ring() -> MidiRing {
        MidiRing::new(1024)
    }

    #[test]
    fn a_note_on_and_off_pair_up() {
        let mut r = ring();
        r.push(100, 0x90, 60, 100);
        r.push(500, 0x80, 60, 64);
        let notes = r.notes_in(0, 1000, false);
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].start, 100);
        assert_eq!(notes[0].end, Some(500));
        assert_eq!(notes[0].on_velocity, 100);
        assert_eq!(notes[0].off_velocity, 64);
    }

    #[test]
    fn a_note_on_with_zero_velocity_is_a_note_off() {
        // What most keyboards actually send. Treating it as an onset gives notes that never stop.
        let mut r = ring();
        r.push(100, 0x90, 60, 100);
        r.push(400, 0x90, 60, 0);
        let notes = r.notes_in(0, 1000, false);
        assert_eq!(notes.len(), 1, "the zero-velocity message must not start a second note");
        assert_eq!(notes[0].end, Some(400));
    }

    #[test]
    fn onset_decides_membership() {
        let mut r = ring();
        // In frame order, as the audio thread always produces them.
        r.push(100, 0x90, 40, 90);   // starts before, rings through
        r.push(300, 0x90, 60, 90);   // squarely inside
        r.push(400, 0x80, 60, 0);
        r.push(700, 0x90, 67, 90);   // starts after
        r.push(800, 0x80, 67, 0);
        r.push(900, 0x80, 40, 0);
        let notes = r.notes_in(200, 600, false);
        let keys: Vec<u8> = notes.iter().map(|n| n.key).collect();
        assert_eq!(keys, vec![60], "only the note that began inside is in it");
    }

    #[test]
    fn a_note_still_sounding_at_the_edge_is_cut_there() {
        let mut r = ring();
        r.push(300, 0x90, 62, 90);
        r.push(900, 0x80, 62, 0);
        let cut = r.notes_in(200, 600, false);
        assert_eq!(cut[0].end, Some(600), "truncated at the boundary");
        let ringing = r.notes_in(200, 600, true);
        assert_eq!(ringing[0].end, Some(900), "let_ring keeps the tail");
    }

    #[test]
    fn a_note_never_released_has_no_end() {
        let mut r = ring();
        r.push(300, 0x90, 64, 80);
        let notes = r.notes_in(200, 600, false);
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].end, None, "still held is not the same as ending at the edge");
    }

    #[test]
    fn re_striking_a_key_before_its_release_pairs_in_order() {
        // Two onsets of the same key, then two releases. Pairing on key alone closes the second
        // note with the first release and leaves the other open forever.
        let mut r = ring();
        r.push(100, 0x90, 60, 100);
        r.push(200, 0x90, 60, 110);
        r.push(300, 0x80, 60, 0);
        r.push(400, 0x80, 60, 0);
        let notes = r.notes_in(0, 1000, false);
        assert_eq!(notes.len(), 2);
        let ends: Vec<Option<u64>> = notes.iter().map(|n| n.end).collect();
        assert!(ends.contains(&Some(300)) && ends.contains(&Some(400)), "got {ends:?}");
        assert!(notes.iter().all(|n| n.end.is_some()), "neither should be left open");
    }

    #[test]
    fn a_release_belonging_to_a_note_outside_the_selection_closes_nothing_inside() {
        let mut r = ring();
        r.push(100, 0x90, 60, 100);  // outside, opens
        r.push(300, 0x90, 60, 100);  // inside
        r.push(350, 0x80, 60, 0);    // closes the inside one, being the most recent
        r.push(900, 0x80, 60, 0);    // closes the outside one
        let notes = r.notes_in(200, 600, false);
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].end, Some(350));
    }

    #[test]
    fn separate_channels_do_not_close_each_other() {
        let mut r = ring();
        r.push(100, 0x90, 60, 100);       // channel 0
        r.push(150, 0x91, 60, 100);       // channel 1, same key
        r.push(200, 0x81, 60, 0);         // channel 1 off
        r.push(300, 0x80, 60, 0);         // channel 0 off
        let notes = r.notes_in(0, 1000, false);
        assert_eq!(notes.len(), 2);
        let ch0 = notes.iter().find(|n| n.channel == 0).expect("channel 0");
        let ch1 = notes.iter().find(|n| n.channel == 1).expect("channel 1");
        assert_eq!(ch0.end, Some(300));
        assert_eq!(ch1.end, Some(200));
    }

    #[test]
    fn the_controller_snapshot_is_the_latest_value_of_each() {
        let mut r = ring();
        r.push(100, 0xB0, 74, 12);
        r.push(200, 0xB0, 64, 127);   // sustain down
        r.push(300, 0xB0, 74, 90);
        r.push(400, 0xB0, 74, 33);    // the one in force
        r.push(700, 0xB0, 74, 100);   // after the boundary, not in the snapshot
        let controls = r.controls_before(500);
        assert_eq!(controls.len(), 2, "one per controller, not one per event");
        let cutoff = controls.iter().find(|c| c.controller == 74).expect("cutoff");
        assert_eq!(cutoff.value, 33);
        let sustain = controls.iter().find(|c| c.controller == 64).expect("sustain");
        assert_eq!(sustain.value, 127, "the pedal was down and the clip must know");
    }

    #[test]
    fn pitch_bend_is_fourteen_bits_and_per_channel() {
        let mut r = ring();
        r.push(100, 0xE0, 0x00, 0x40);   // centre on channel 0
        r.push(200, 0xE1, 0x7F, 0x7F);   // full up on channel 1
        r.push(300, 0xE0, 0x10, 0x4E);
        let bends = r.bends_before(500);
        assert_eq!(bends.len(), 2);
        assert_eq!(bends.iter().find(|(c, _)| *c == 0).expect("ch0").1, 0x10 | (0x4E << 7));
        assert_eq!(bends.iter().find(|(c, _)| *c == 1).expect("ch1").1, 16_383);
    }

    #[test]
    fn the_ring_overwrites_and_keeps_the_newest() {
        let mut r = MidiRing::new(16);
        for i in 0..100u64 {
            r.push(i * 10, 0x90, 60, 100);
        }
        let held: Vec<Event> = r.iter().collect();
        assert_eq!(held.len(), 16, "a full ring holds exactly its capacity");
        assert_eq!(held[0].frame, 84 * 10, "and it is the newest sixteen");
        assert_eq!(held[15].frame, 99 * 10);
    }

    #[test]
    fn system_messages_are_not_captured() {
        let mut r = ring();
        r.push(100, 0xF8, 0, 0);   // clock
        r.push(100, 0xFA, 0, 0);   // start
        r.push(100, 0x90, 60, 100);
        assert_eq!(r.iter().count(), 1, "transport arrives through the clock, not the lane");
    }
}
