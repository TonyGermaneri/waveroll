//! Device acquisition, and a `block_on` small enough to not be a dependency.
//!
//! wgpu's adapter and device requests are futures even on native backends, where they resolve
//! essentially immediately. Pulling in an async runtime — or even `pollster` — to await two calls
//! that never really suspend is a poor trade for something that will ship inside other people's
//! DAWs, where every crate in the tree is a thing somebody has to account for. Thirty lines of
//! `std::task` does it.

use std::future::Future;
use std::pin::pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

struct ThreadWaker(std::thread::Thread);

impl Wake for ThreadWaker {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

/// Drives a future to completion on the calling thread.
///
/// Parks with a timeout rather than indefinitely: a backend that completes work without waking the
/// waker would otherwise hang here forever, and a spurious ten-millisecond wakeup costs nothing on
/// a path that runs a handful of times per process.
pub fn block_on<F: Future>(future: F) -> F::Output {
    let waker = Waker::from(Arc::new(ThreadWaker(std::thread::current())));
    let mut context = Context::from_waker(&waker);
    let mut future = pin!(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::park_timeout(Duration::from_millis(10)),
        }
    }
}

/// A device and its queue, plus what we were actually given.
pub struct Gpu {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    errors: Arc<Mutex<Vec<String>>>,
    /// Kept because surface capabilities are asked of the adapter, not the device, and a caller
    /// that has one of these should not have to have held on to the other.
    pub adapter: wgpu::Adapter,
    pub info: wgpu::AdapterInfo,
}

impl Gpu {
    /// Opens a device, optionally one able to present to `surface`.
    ///
    /// Async because it has to be: under wasm there is no thread to block, and the same call has
    /// to serve the browser, the plugin editor whose surface belongs to the host, and CI with no
    /// surface at all. [`Gpu::headless`] is the blocking convenience for the last of those.
    pub async fn open(
        instance: &wgpu::Instance,
        compatible_surface: Option<&wgpu::Surface<'_>>,
    ) -> Result<Gpu, String> {
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                force_fallback_adapter: false,
                compatible_surface,
            })
            .await
            .map_err(|e| format!("no GPU adapter: {e}"))?;
        let info = adapter.get_info();

        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("waveroll"),
                // Nothing here needs an optional feature. Asking for none keeps the same code path
                // on WebGPU, where most of them do not exist.
                required_features: wgpu::Features::empty(),
                // `defaults()` rather than `downlevel_defaults()`, and the difference is not
                // academic: downlevel caps a 2D texture at 2048 px, which a resizable plugin
                // editor passes at about 1024 points on a 2x display. The surface then fails
                // validation on resize. `defaults()` allows 8192 and is still the conservative
                // WebGPU baseline rather than whatever this particular adapter happens to offer.
                required_limits: wgpu::Limits::default().using_resolution(adapter.limits()),
                ..Default::default()
            })
            .await
            .map_err(|e| format!("no GPU device: {e}"))?;

        // Recorded, never panicked on.
        //
        // This used to panic, on the reasoning that a validation error otherwise turns "the shader
        // did not run" into "the output buffer is still zeros" -- a wrong answer rather than a
        // failure. That reasoning holds in a test and is catastrophic in a plugin: the panic
        // crosses the C boundary, Rust aborts because it cannot unwind through it, and the host
        // dies. It took Ableton down. The error is kept for whoever asks instead, and tests assert
        // on it explicitly.
        let errors = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&errors);
        device.on_uncaptured_error(Box::new(move |error| {
            if let Ok(mut sink) = sink.lock() {
                // Bounded: a failure that repeats every frame must not become a memory leak.
                if sink.len() < 32 {
                    sink.push(error.to_string());
                }
            }
        }));

        Ok(Gpu { device, queue, adapter, info, errors })
    }

    /// Opens a device with no surface attached, blocking until it is ready. Native only — under
    /// wasm there is no thread to block and [`Gpu::open`] must be awaited instead.
    pub fn headless() -> Result<Gpu, String> {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
        block_on(Gpu::open(&instance, None))
    }

    /// Validation errors seen since the last call, and clears them.
    ///
    /// Something has to look at these or they are no better than the silence they replaced.
    pub fn take_errors(&self) -> Vec<String> {
        self.errors.lock().map(|mut e| std::mem::take(&mut *e)).unwrap_or_default()
    }

    /// The largest surface this device will accept, in pixels.
    ///
    /// Asked rather than assumed: a window can be dragged bigger than any limit, and a surface
    /// configured past one is a validation error on a path with no way to report it.
    pub fn max_surface(&self) -> u32 {
        self.device.limits().max_texture_dimension_2d
    }

    pub fn describe(&self) -> String {
        format!("{} ({:?}, {:?})", self.info.name, self.info.backend, self.info.device_type)
    }
}
