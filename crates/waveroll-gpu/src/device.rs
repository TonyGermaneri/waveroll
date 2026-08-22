//! Device acquisition, and a `block_on` small enough to not be a dependency.
//!
//! wgpu's adapter and device requests are futures even on native backends, where they resolve
//! essentially immediately. Pulling in an async runtime — or even `pollster` — to await two calls
//! that never really suspend is a poor trade for something that will ship inside other people's
//! DAWs, where every crate in the tree is a thing somebody has to account for. Thirty lines of
//! `std::task` does it.

use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
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
                // downlevel_defaults rather than the adapter's own limits, so a shader that only
                // works on a generous desktop GPU fails here rather than in a browser.
                required_limits: wgpu::Limits::downlevel_defaults(),
                ..Default::default()
            })
            .await
            .map_err(|e| format!("no GPU device: {e}"))?;

        // A validation error inside a compute pass is otherwise reported by wgpu and then ignored,
        // which turns "the shader did not run" into "the output buffer is still zeros" — a wrong
        // answer rather than a failure.
        device.on_uncaptured_error(Box::new(|error| {
            panic!("wgpu validation error: {error}");
        }));

        Ok(Gpu { device, queue, adapter, info })
    }

    /// Opens a device with no surface attached, blocking until it is ready. Native only — under
    /// wasm there is no thread to block and [`Gpu::open`] must be awaited instead.
    pub fn headless() -> Result<Gpu, String> {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
        block_on(Gpu::open(&instance, None))
    }

    pub fn describe(&self) -> String {
        format!("{} ({:?}, {:?})", self.info.name, self.info.backend, self.info.device_type)
    }
}
