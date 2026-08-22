//! The analysis and render chain, on `wgpu`.
//!
//! `wgpu` is the portability layer: it compiles to WebGPU under wasm and to Metal, D3D12 and
//! Vulkan natively, from one Rust source and one set of WGSL. That is what lets the browser build,
//! the standalone app and the plugin draw the same picture with the same code, rather than a
//! TypeScript renderer and a native one that drift.
//!
//! The shaders in `src/shaders/` are vendored from waveshape essentially unchanged, because WGSL
//! is `wgpu`'s own shading language and they already targeted WebGPU.

pub mod device;
pub mod envelope;
pub mod fft;
pub mod reference;

pub use device::Gpu;
pub use envelope::{Envelope, EnvelopePass, RingMirror};
pub use fft::Analyzer;
