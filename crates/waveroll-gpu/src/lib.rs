//! The analysis and render chain, on `wgpu`.
//!
//! `wgpu` is the portability layer: it compiles to WebGPU under wasm and to Metal, D3D12 and
//! Vulkan natively, from one Rust source and one set of WGSL. That is what lets the browser build,
//! the standalone app and the plugin draw the same picture with the same code, rather than a
//! TypeScript renderer and a native one that drift.
//!
//! The shaders in `src/shaders/` are vendored from waveshape essentially unchanged, because WGSL
//! is `wgpu`'s own shading language and they already targeted WebGPU.

/// Re-exported so dependents cannot end up linking a second, incompatible wgpu. Two versions in
/// one binary compile perfectly and then fail at the first type that crosses between them.
pub use wgpu;

pub mod device;
pub mod envelope;
pub mod fft;
pub mod overlay;
pub mod reference;
pub mod render;

pub use device::Gpu;
pub use envelope::{Envelope, EnvelopePass, RingMirror};
pub use fft::Analyzer;
pub use overlay::{Overlay, OverlayPass};
pub use render::{OverlayStyle, Style, Target, WaveformPass};
