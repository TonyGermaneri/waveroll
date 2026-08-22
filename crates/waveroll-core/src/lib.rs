//! Waveroll's core: everything that has to behave identically in a browser, in a desktop app and
//! inside a plugin host.
//!
//! There is no I/O here, no UI, and nothing platform-specific. That is not tidiness for its own
//! sake — it is what lets the same ring, the same grid arithmetic and the same file writers serve
//! a wasm build, a JUCE standalone and an AU/VST3/CLAP plugin without a second implementation to
//! keep in step.

pub mod clock;
pub mod grid;
pub mod ring;
pub mod tempo;

pub use clock::{CaptureClock, ClockPll, ClockSource, Transport};
pub use grid::{Selection, Unit};
pub use ring::{Producer, Reader, ring};
pub use tempo::{Meter, TempoMap};
